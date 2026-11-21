// Build independently of the main entry point, like tests/locator.cpp.
#include "../ingest.cpp"
#include <cassert>
#include <cstdio>

int main() {
    // At this cardinality the owner table has 128 slots. Find 49 distinct
    // nine-byte IDs with one common home slot to exceed its 48-probe bound.
    std::vector<Key> keys;
    for (unsigned i = 0; keys.size() < 49; ++i) {
        char text[16];
        const int length = std::snprintf(text, sizeof(text), "e%08u", i);
        assert(length == 9);
        const Key key = Key::from({text, static_cast<size_t>(length)});
        if ((filterHash(key) & 127u) == 0) keys.push_back(key);
    }

    Source source;
    std::vector<std::unique_ptr<Reader>> readers;
    readers.push_back(std::make_unique<Reader>(1, 1));
    readers.push_back(std::make_unique<Reader>(1, 1));
    auto valid = [&](size_t r, Key id, int64_t arrival) {
        Reader& reader = *readers[r];
        const uint32_t index = reader.events.add(0, Event::pack(Key::from("buyer"), 0, {1, 0, 0}));
        reader.ids.add(0, {id, location(reader, arrival, 0, 0, index)});
        return index;
    };
    auto invalid = [&](size_t r, Key id, int64_t arrival) {
        readers[r]->ids.add(0, {id, (relativeArrival(arrival, 0) << 32) | invalidArrivalTag});
    };
    auto alive = [&](size_t r, uint32_t index) {
        return !readers[r]->events.at(0, index).person.empty();
    };

    const auto provisional = valid(0, keys[0], 20);
    const auto late = valid(0, keys[0], 30);
    invalid(0, keys[1], 5);
    const auto suppressed = valid(0, keys[1], 19);
    std::vector<uint32_t> unique;
    for (size_t i = 2; i < keys.size(); ++i) unique.push_back(valid(0, keys[i], 20));

    const auto earliest = valid(1, keys[0], 10);
    invalid(1, keys[0], 25);
    const auto laterThanInvalid = valid(1, keys[1], 8);
    invalid(1, keys.back(), 5);
    for (auto& reader : readers) {
        reader->events.finish();
        reader->ids.finish();
    }
    const size_t count = readers[0]->ids.count(0) + readers[1]->ids.count(0);
    assert(std::bit_ceil(count * 2) == 128);
    const bool finished = compactOwners(0, count, 0, source, readers);
    assert(!finished);
    assert(alive(0, provisional) && !alive(0, late) && !alive(0, suppressed));
    assert(alive(0, unique.back()) && alive(1, laterThanInvalid));

    deduplicate(0, 0, source, readers);
    assert(!alive(0, provisional) && !alive(0, late) && alive(1, earliest));
    assert(!alive(0, suppressed) && !alive(1, laterThanInvalid));
    assert(!alive(0, unique.back()));
    for (size_t i = 0; i + 1 < unique.size(); ++i) assert(alive(0, unique[i]));
}
