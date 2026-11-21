use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;

use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::parser::parse_message_type;

use crate::input::Dimensions;
use crate::{CampaignTotals, Result};

pub struct UserDay<'a> {
    pub person: &'a [u8],
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub category: Option<u32>,
}

fn writer(path: &Path, schema: &str) -> Result<SerializedFileWriter<File>> {
    fs::create_dir_all(path.parent().unwrap())?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    Ok(SerializedFileWriter::new(
        File::create(path)?,
        Arc::new(parse_message_type(schema)?),
        Arc::new(properties),
    )?)
}

fn strings<'a>(
    group: &mut SerializedRowGroupWriter<'_, File>,
    values: impl Iterator<Item = &'a [u8]>,
) -> Result<()> {
    let values: Vec<_> = values.map(ByteArray::from).collect();
    let mut column = group.next_column()?.unwrap();
    column
        .typed::<ByteArrayType>()
        .write_batch(&values, None, None)?;
    column.close()?;
    Ok(())
}

fn integers(
    group: &mut SerializedRowGroupWriter<'_, File>,
    values: impl Iterator<Item = i64>,
) -> Result<()> {
    let values: Vec<_> = values.collect();
    let mut column = group.next_column()?.unwrap();
    column
        .typed::<Int64Type>()
        .write_batch(&values, None, None)?;
    column.close()?;
    Ok(())
}

fn dates(
    group: &mut SerializedRowGroupWriter<'_, File>,
    values: impl Iterator<Item = i32>,
) -> Result<()> {
    let values: Vec<_> = values.collect();
    let mut column = group.next_column()?.unwrap();
    column
        .typed::<Int32Type>()
        .write_batch(&values, None, None)?;
    column.close()?;
    Ok(())
}

pub fn users(path: &Path, rows: &[UserDay<'_>], dimensions: &Dimensions) -> Result<()> {
    let mut file = writer(
        path,
        "message user_daily {
        REQUIRED BINARY user_id (UTF8);
        REQUIRED INT32 day (DATE);
        REQUIRED INT64 sessions;
        REQUIRED INT64 events;
        REQUIRED INT64 revenue_cents;
        OPTIONAL BINARY top_category (UTF8);
    }",
    )?;
    for batch in rows.chunks(65536) {
        let mut group = file.next_row_group()?;
        strings(&mut group, batch.iter().map(|row| row.person))?;
        dates(&mut group, batch.iter().map(|row| row.day))?;
        integers(&mut group, batch.iter().map(|row| row.sessions))?;
        integers(&mut group, batch.iter().map(|row| row.events))?;
        integers(&mut group, batch.iter().map(|row| row.revenue))?;
        let levels: Vec<i16> = batch
            .iter()
            .map(|row| i16::from(row.category.is_some()))
            .collect();
        let values: Vec<_> = batch
            .iter()
            .filter_map(|row| row.category)
            .map(|category| ByteArray::from(dimensions.categories[category as usize].as_slice()))
            .collect();
        let mut column = group.next_column()?.unwrap();
        column
            .typed::<ByteArrayType>()
            .write_batch(&values, Some(&levels), None)?;
        column.close()?;
        group.close()?;
    }
    file.close()?;
    Ok(())
}

pub fn campaigns(path: &Path, totals: &CampaignTotals, dimensions: &Dimensions) -> Result<()> {
    let mut rows: Vec<_> = totals.iter().collect();
    rows.sort_unstable_by_key(|(key, _)| **key);
    let mut file = writer(
        path,
        "message campaign_daily {
        REQUIRED BINARY campaign_id (UTF8);
        REQUIRED BINARY channel (UTF8);
        REQUIRED INT32 day (DATE);
        REQUIRED INT64 sessions;
        REQUIRED INT64 purchasing_sessions;
        REQUIRED INT64 revenue_cents;
        REQUIRED INT64 assisted_sessions;
    }",
    )?;
    for batch in rows.chunks(65536) {
        let mut group = file.next_row_group()?;
        strings(
            &mut group,
            batch
                .iter()
                .map(|((campaign, _), _)| dimensions.campaign_names[*campaign as usize].as_slice()),
        )?;
        strings(
            &mut group,
            batch
                .iter()
                .map(|((campaign, _), _)| dimensions.channels[*campaign as usize].as_slice()),
        )?;
        dates(&mut group, batch.iter().map(|((_, day), _)| *day))?;
        integers(&mut group, batch.iter().map(|(_, stats)| stats.sessions))?;
        integers(
            &mut group,
            batch.iter().map(|(_, stats)| stats.purchasing_sessions),
        )?;
        integers(&mut group, batch.iter().map(|(_, stats)| stats.revenue))?;
        integers(&mut group, batch.iter().map(|(_, stats)| stats.assists))?;
        group.close()?;
    }
    file.close()?;
    Ok(())
}
