// Compile on x86-64 with -std=c++20 -O2 -march=haswell -ffunction-sections
// and -Wl,--gc-sections to exercise this translation unit in isolation.
#include "../ingest.cpp"
#include <cassert>

int main() {
    Reader reader(256, 1);
    Source source;
    for (int64_t start : {-dayMs, int64_t(0), 20000 * dayMs}) {
        const int64_t origin = orderOrigin(start);
        // Distinct days and the last valid clock exercise both parts of the rank.
        for (uint64_t rank : {uint64_t(0), uint64_t(0x235959), uint64_t(1023) * clockStride + 0x235959,
                              missingOrder}) {
            const int64_t time = rank == missingOrder ? INT64_MAX : decodeOrder(rank, origin);
            ParsedEvent row{.order = rank, .arrival = time};
            for (uint32_t bucket : {0u, 1u, 511u, 1023u}) {
                for (uint32_t index : {0u, indexLimit - 1, indexLimit, UINT32_MAX}) {
                    const uint64_t info = location(reader, row, origin, bucket, index);
                    assert(arrival(info, reader, source, origin) == time);
                    const bool fits = rank != missingOrder && index < indexLimit;
                    assert((uint32_t(info) < sourceTag) == fits);
                    if (fits) {
                        assert((uint32_t(info) >> indexBits) == bucket);
                        assert((info & (indexLimit - 1)) == index);
                    } else {
                        const auto& extended = reader.slow.at(specialValue(info));
                        assert(extended.bucket == bucket && extended.index == index);
                    }
                }
            }
        }
    }
    for (uint64_t offset : {0ULL, 0xffffffffULL, 0x100000000ULL, 0xffffffffffULL}) {
        for (uint32_t tag : {sourceTag, slowTag})
            assert(specialValue(special(offset, tag)) == offset);
    }
}
