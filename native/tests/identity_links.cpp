#include "../shared.hpp"

#include <iostream>

namespace {
using Files = std::array<std::string, 8>;

Files inputs() {
    Files files;
    files.fill("user_id_a,user_id_b\n");
    return files;
}

void edge(std::string& file, const std::string& a, const std::string& b) {
    file += a + "," + b + "\n";
}

Context resolve(const Files& files, unsigned workers) {
    std::vector<std::string_view> views(files.begin(), files.end());
    Context result{};
    linkPeople(views, result, workers);
    return result;
}

void check(const Context& ctx, const std::string& name, const std::string& expected) {
    const Key raw = Key::from(name);
    uint64_t hash = raw.hash();
    const Key actual = ctx.identities.person(raw, hash);
    Key::Buffer buffer;
    if (actual.view(buffer) != expected || hash != Key::from(expected).hash())
        throw std::runtime_error("Incorrect canonical identity or hash for " + name);
}

void checkChain(unsigned workers) {
    Files files = inputs();
    std::vector<std::string> names;
    for (int i = 0; i < 2048; ++i)
        names.push_back("k" + std::to_string(100000 + i));
    for (size_t i = 1; i < names.size(); ++i) {
        // Each edge has a distinct larger endpoint, so the initial forest can
        // contain a long chain even when the eight batches run out of order.
        edge(files[i % files.size()], names[i], names[i-1]);
    }
    // The same child receives two possible parents in different files.
    // Repeated external spellings are parsed from different addresses.
    const std::string hub = "zz-hub-with-a-long-spelling";
    const std::string a = "aa-left";
    const std::string b = "ab-right";
    edge(files[0], hub, a);
    edge(files[1], hub, b);
    edge(files[2], names.back(), b);
    edge(files[3], b, hub);
    edge(files[4], a, a);
    edge(files[5], "second-a", "second-b");
    const auto ctx = resolve(files, workers);
    for (const auto& name : names) check(ctx, name, a);
    for (const auto& name : {a, b, hub}) check(ctx, name, a);
    check(ctx, "second-b", "second-a");
    check(ctx, "absent", "absent");
}

void checkCrowdedShard(size_t members) {
    Files files = inputs();
    std::vector<std::string> names;
    for (unsigned i = 0; names.size() < members + 128; ++i) {
        auto name = "n" + std::to_string(i);
        if (Identities::shard(Key::from(name).hash()) == 17)
            names.push_back(std::move(name));
    }
    const std::string root = "!";
    for (size_t i = 0; i < members; ++i)
        edge(files[i % files.size()], names[i], root);
    const auto ctx = resolve(files, 8);
    const auto& dict = ctx.identities;
    const auto start = dict.slots.begin() + 17 * dict.shardSize;
    // A failed probe on a completely full shard could otherwise loop forever.
    if (std::none_of(start, start + dict.shardSize, [](const auto& a) { return a.raw.empty(); }))
        throw std::runtime_error("Crowded shard lacks an empty probe terminator");
    for (size_t i = 0; i < names.size(); ++i)
        check(ctx, names[i], i < members ? root : names[i]);
    check(ctx, root, root);
}
}  // namespace

int main() {
    checkChain(1);
    for (int repetition = 0; repetition < 12; ++repetition) checkChain(8);
    // The initial fast-path capacity is 128 for each input. Exercise both a
    // successfully inserted final slot and an insertion that exhausts probes.
    checkCrowdedShard(128);
    checkCrowdedShard(192);
    std::cout << "Identity chains, conflicting parents and shard overflow passed.\n";
}
