fn run(arguments: &Arguments) -> io::Result<()> {
    let dataset = Dataset::plan(arguments.seed, arguments.events, arguments.clicks);
    let output = &arguments.output;
    let specifications = [
        ("events", dataset.event_parts.len(), EVENT_HEADER, Stream::ShuffleEvents),
        ("clicks", dataset.click_parts.len(), CLICK_HEADER, Stream::ShuffleClicks),
        ("links", dataset.link_parts.len(), LINK_HEADER, Stream::ShuffleLinks),
    ];
    let mut writers: Vec<Vec<BufWriter<File>>> = Vec::new();
    for (folder, count, header, _) in &specifications {
        fs::create_dir_all(output.join(folder))?;
        let mut group = Vec::new();
        for part in 0..*count {
            let mut file = BufWriter::new(File::create(output.join(folder).join(format!("part-{part:05}.csv")))?);
            writeln!(file, "{header}")?;
            group.push(file);
        }
        writers.push(group);
    }
    for (index, users) in dataset.shards.iter().enumerate() {
        let shard = generate_shard(&dataset, users.clone());
        for (group, parts) in writers.iter_mut().zip([shard.events, shard.clicks, shard.links]) {
            for (writer, bytes) in group.iter_mut().zip(parts.bytes) {
                writer.write_all(&bytes)?;
            }
        }
        if index % 64 == 0 { eprintln!("generated shard {} of {}", index, dataset.shards.len()); }
    }
    for group in &mut writers { for file in group { file.flush()?; } }
    drop(writers);
    for (folder, count, header, stream) in specifications {
        for part in 0..count {
            let path = output.join(folder).join(format!("part-{part:05}.csv"));
            let bytes = fs::read(&path)?;
            let mut rows: Vec<&[u8]> = bytes[header.len() + 1..].split_inclusive(|&byte| byte == b'\n').collect();
            let mut random = Random::new(dataset.seed, stream, part as u64);
            random.shuffle(&mut rows);
            let mut writer = BufWriter::with_capacity(1 << 20, File::create(&path)?);
            writeln!(writer, "{header}")?;
            for row in rows { writer.write_all(row)?; }
            writer.flush()?;
        }
    }
    write_lookups(output, &dataset)
}
