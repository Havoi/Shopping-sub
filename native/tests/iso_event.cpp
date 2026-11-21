#include "../ingest.cpp"

#include <cstdio>
#include <iostream>
#include <random>

namespace {
bool matchesGeneric(const std::string& input, const std::string& arrival,
                    int64_t earliest, const Context& context) {
    const int64_t expected = timestamp(input);
    const bool shouldAccept = expected >= context.start && expected < context.end && expected >= earliest;
    ParsedEvent row{};
    row.order = missingOrder;
    row.arrival = invalidTime;
    int64_t actual = 0;
    const bool accepted = eventTimes(input, arrival, context, actual, row);
    if (accepted == shouldAccept && (!accepted || actual == expected)) return true;

    std::cerr << "Fast parser accepted " << accepted << " with timestamp " << actual
              << ", expected acceptance " << shouldAccept << " and timestamp " << expected << '\n';
    return false;
}

std::string randomOffsetTimestamp(std::mt19937& random) {
    const unsigned day = 1 + random() % 7;
    // Sample past each legal limit to cover invalid local times and offsets.
    const unsigned hour = random() % 25;
    const unsigned minute = random() % 62;
    const unsigned second = random() % 62;
    const char sign = random() % 2 ? '+' : '-';
    const unsigned zoneHour = random() % 25;
    const unsigned zoneMinute = random() % 62;
    char text[40];
    std::snprintf(text, sizeof text, "2025-01-0%uT%02u:%02u:%02u%c%02u:%02u",
                  day, hour, minute, second, sign, zoneHour, zoneMinute);
    return text;
}
}  // namespace

int main() {
    Context context{};
    context.start = -100'000'000'000'000LL;
    context.end = 100'000'000'000'000LL;
    readerOrderOrigin = orderOrigin(context.start);
    const std::string arrival = "2024-12-31T02:03:04Z";
    const int64_t earliest = timestamp(arrival) - dayMs;

    int checks = 0;
    for (const std::string original : {"2025-01-01T03:06:47Z", "2025-01-01T03:06:47+08:30",
                                       "2025-01-01T03:06:47-08:30"}) {
        for (size_t position = 0; position < original.size(); ++position) {
            for (int byte = 0; byte < 256; ++byte) {
                std::string input = original;
                input[position] = static_cast<char>(byte);
                if (!matchesGeneric(input, arrival, earliest, context)) {
                    std::cerr << "Failed mutation of " << original << " at position " << position
                              << " with byte " << byte << '\n';
                    return 1;
                }
                ++checks;
            }
        }
    }

    std::mt19937 random(8193);
    for (int trial = 0; trial < 50'000; ++trial) {
        const std::string input = randomOffsetTimestamp(random);
        if (!matchesGeneric(input, arrival, earliest, context)) {
            std::cerr << "Failed offset timestamp " << input << " on trial " << trial << '\n';
            return 1;
        }
        ++checks;
    }
    std::cout << checks << " event timestamp checks passed\n";
}
