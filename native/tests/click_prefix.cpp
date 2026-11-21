#include "../ingest.cpp"

#include <cstdlib>
#include <iostream>
#include <random>

namespace {
void checkClockMutations() {
    const std::string original = "2025-01-01T23:07:40Z";
    for (size_t position = 0; position < original.size(); ++position) {
        for (int byte = 0; byte < 256; ++byte) {
            std::string input = original;
            input[position] = static_cast<char>(byte);
            const int64_t expected = timestamp(input);
            const int64_t actual = clickTime(input.data(), input.size());
            if (actual != expected) {
                std::cerr << "UTC timestamp mutation at position " << position << ", byte " << byte
                          << " produced " << actual << ", expected " << expected << '\n';
                std::exit(1);
            }
        }
    }
}

std::string randomClick(std::mt19937& random) {
    const std::string id(random() % 24, 'i');
    const std::string user(random() % 18, 'u');
    std::string campaign;
    if (random() % 7) campaign = random() % 2 ? "c_012" : "x";

    // Include each supported format, an impossible date and an empty field.
    static const std::string times[] = {
        "2025-01-01T23:59:59Z", "1735775999000", "2025-01-01T00:00:00.030+01:00",
        "2025-01-01T00:00:00Z", "00000001735775999000", "1735775999", "2025-09-31T22:33:44Z", ""
    };
    auto field = [&](const std::string& value) {
        return random() % 2 ? '"' + value + '"' : value;
    };
    std::string line = field(id);
    line += ',' + field(user);
    line += ',' + field(campaign);
    line += ',' + field(times[random() % std::size(times)]);
    if (random() % 3 == 0) {
        const char replacement = "\",!x"[random() % 4];
        line[random() % line.size()] = replacement;
    }
    return line;
}

bool sameClick(const Click& actual, const Click& expected) {
    return actual.id == expected.id && actual.person == expected.person &&
           actual.ts == expected.ts && actual.campaign == expected.campaign;
}
}  // namespace

int main() {
    checkClockMutations();
    Context context{};
    context.start = 0;
    context.end = 4'000'000'000'000LL;
    context.campaigns.insert(Key::from("x"), 1);
    context.campaigns.insert(Key::from("c_012"), 2);
    context.campaigns.finish();

    std::mt19937 random(127);
    constexpr int trials = 40'000;
    int accepted = 0;
    for (int trial = 0; trial < trials; ++trial) {
        const std::string line = randomClick(random);
        Click actual{}, expected{};
        const bool fastAccepted = parseClick(line, context, actual);
        const bool genericAccepted = otherClick(line, context, expected);
        if (fastAccepted != genericAccepted || (fastAccepted && !sameClick(actual, expected))) {
            std::cerr << "Click mismatch on trial " << trial << ", input [" << line << "]\n"
                      << "Fast parser accepted " << fastAccepted << ", generic parser accepted "
                      << genericAccepted << ", timestamps " << actual.ts << '/' << expected.ts
                      << ", campaigns " << actual.campaign << '/' << expected.campaign << '\n';
            return 1;
        }
        accepted += fastAccepted;
    }
    std::cout << "5120 UTC timestamp mutations and " << trials << " click rows passed ("
              << accepted << " accepted)\n";
}
