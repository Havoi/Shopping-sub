#include "../ingest.cpp"

#include <cstdio>
#include <cstdlib>
#include <iostream>

namespace {
constexpr int64_t january2025 = 1'735'689'600'000LL;

std::string utc(const char* day, unsigned seconds) {
    char text[48];
    std::snprintf(text, sizeof text, "%sT%02u:%02u:%02uZ", day, seconds / 3600,
                  seconds / 60 % 60, seconds % 60);
    return text;
}

ParsedEvent emptyEvent() {
    ParsedEvent row{};
    row.order = missingOrder;
    row.arrival = invalidTime;
    return row;
}

void require(bool passed, std::string_view condition, std::string_view input) {
    if (!passed) {
        std::cerr << condition << " failed for " << input << '\n';
        std::exit(1);
    }
}

void checkEverySecond(const char* day, int64_t midnight) {
    Context context{};
    context.start = midnight - dayMs;
    context.end = midnight + 2 * dayMs;
    readerOrderOrigin = orderOrigin(context.start);

    uint64_t previousOrder = 0;
    for (unsigned second = 0; second < 86'400; ++second) {
        const std::string time = utc(day, second);
        ParsedEvent row = emptyEvent();
        int64_t parsed = 0;
        require(eventTimes(time, time, context, parsed, row), "UTC timestamp acceptance", time);
        require(parsed == midnight + 1000LL * second, "Unix millisecond conversion", time);
        require(row.order <= UINT32_MAX, "Compact arrival representation", time);
        require(decodeOrder(row.order, readerOrderOrigin) == parsed, "Arrival round trip", time);
        require(second == 0 || row.order > previousOrder, "Strict chronological ordering", time);
        previousOrder = row.order;
    }
}

void checkLatenessBoundary(bool compact) {
    Context context{};
    // A 1970 window start puts the 2025 arrival outside the compact range.
    context.start = compact ? january2025 - 3 * dayMs : 0;
    context.end = january2025 + 4 * dayMs;
    readerOrderOrigin = orderOrigin(context.start);

    for (unsigned second : {0u, 1u, 3601u, 86398u, 86399u}) {
        const std::string arrival = utc("2025-01-01", second);
        const int64_t arrivalMs = january2025 + 1000LL * second;
        for (int64_t delta : {-1LL, 0LL, 1LL, 1000LL, 86'400'000LL}) {
            const int64_t expected = arrivalMs - dayMs + delta;
            const std::string timestampDigits = std::to_string(expected);
            // The 13-digit fast path can read three bytes from the next CSV field.
            const std::string csv = timestampDigits + ",pad";
            const auto field = std::string_view(csv).substr(0, timestampDigits.size());
            ParsedEvent row = emptyEvent();
            int64_t parsed = 0;
            const bool accepted = eventTimes(field, arrival, context, parsed, row);
            if (accepted != (delta >= 0) || parsed != expected) {
                std::cerr << "24-hour cutoff failed for arrival " << arrival << ", offset "
                          << delta << " ms, compact " << compact << ", acceptance " << accepted
                          << ", parsed timestamp " << parsed << ", expected " << expected << '\n';
                std::exit(1);
            }
            if (compact) {
                require(row.order <= UINT32_MAX && decodeOrder(row.order, readerOrderOrigin) == arrivalMs,
                        "Compact arrival at the cutoff", arrival);
            } else {
                require(row.order == missingOrder && row.arrival == arrivalMs,
                        "Full-width arrival at the cutoff", arrival);
            }
        }
    }

    const std::string arrival = "2025-01-01T23:59:59Z";
    const std::string onBoundary = "2024-12-31T22:59:59-01:00";
    const std::string tooEarly = "2024-12-31T22:59:58-01:00";
    int64_t parsed = 0;
    ParsedEvent row = emptyEvent();
    require(eventTimes(onBoundary, arrival, context, parsed, row), "UTC offset at cutoff", onBoundary);
    require(parsed == january2025 - 1000, "UTC conversion at cutoff", onBoundary);
    require(!eventTimes(tooEarly, arrival, context, parsed, row), "UTC offset before cutoff", tooEarly);
}

void checkMixedRepresentations() {
    const int64_t origin = orderOrigin(-dayMs);
    readerOrderOrigin = origin;
    Reader reader(1, 1);
    Source source;
    const std::string csv = "x,u,0,1969-12-31T23:59:59Z,view,p,\n";
    source.files.push_back(csv);
    source.starts.push_back(0);
    ParsedEvent row = emptyEvent();
    rankArrival("1970-01-01T00:00:00Z", row);
    const uint64_t compact = row.order << 32;
    reader.slow.push_back({-1, 0, 1});
    reader.slow.push_back({1, 0, 2});

    struct Sample {
        const char* name;
        uint64_t location;
        int64_t milliseconds;
    };
    const Sample samples[] = {
        {"source before epoch", special(0, sourceTag), -1000},
        {"full-width before epoch", special(0, slowTag), -1},
        {"compact at epoch", compact, 0},
        {"rejected at epoch", compact | invalidArrivalTag, 0},
        {"full-width after epoch", special(1, slowTag), 1}
    };
    for (const auto& first : samples) {
        for (const auto& second : samples) {
            const bool earlier = earlierArrival(first.location, reader, second.location, reader, source, origin);
            if (earlier != (first.milliseconds < second.milliseconds)) {
                std::cerr << "Arrival ordering failed for [" << first.name << "] before ["
                          << second.name << "]\n";
                std::exit(1);
            }
        }
    }
}
}  // namespace

int main() {
    checkEverySecond("1969-12-31", -dayMs);
    checkEverySecond("2000-02-29", 951'782'400'000LL);
    checkEverySecond("2025-01-01", january2025);
    checkLatenessBoundary(false);
    checkLatenessBoundary(true);
    checkMixedRepresentations();
    std::cout << "259200 UTC clocks, inclusive lateness cutoffs and mixed arrival comparisons passed\n";
}
