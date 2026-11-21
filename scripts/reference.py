#!/usr/bin/env python3
"""Independent small-data oracle for differential testing.

Uses graph traversal, Python datetime, and exhaustive click matching. Run as
python scripts/reference.py INPUT EXPECTED to write the two expected CSVs.
"""
from collections import defaultdict
import csv
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import re
import sys


DAY = 86_400_000
WEEK = 7 * DAY
EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)
DECIMAL = re.compile(rb"[0-9]+")
ISO = re.compile(rb"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(Z|[+-][0-9]{2}:[0-9]{2})")


def timestamp(value):
    if DECIMAL.fullmatch(value):
        # Bound conversion work for arbitrarily long, out-of-window F3 values.
        significant = value.lstrip(b"0") or b"0"
        return int(significant) if len(significant) <= 19 else 10**30
    if not ISO.fullmatch(value):
        return None
    if len(value) == 25 and (int(value[20:22]) > 23 or int(value[23:25]) > 59):
        return None
    try:
        instant = datetime.fromisoformat(value.decode().replace("Z", "+00:00"))
        delta = instant - EPOCH
        return delta.days * DAY + delta.seconds * 1000
    except ValueError:
        return None


def read_rows(paths, width):
    for path in paths:
        with path.open("rb") as file:
            next(file, None)
            for line in file:
                fields = line.removesuffix(b"\n").split(b",")
                if len(fields) != width:
                    continue
                row = []
                for field in fields:
                    if field.startswith(b'"') and len(field) >= 2 and field.endswith(b'"'):
                        field = field[1:-1]
                    if b'"' in field:
                        break
                    row.append(field)
                if len(row) == width:
                    yield row


def solve(data):
    manifest = json.loads((data / "manifest.json").read_text())
    start = timestamp(manifest["window_start"].encode())
    end = timestamp(manifest["window_end"].encode())
    products = dict(read_rows([data / "products.csv"], 2))
    campaigns = dict(read_rows([data / "campaigns.csv"], 2))
    graph = defaultdict(set)
    for left, right in read_rows(sorted((data / "links").glob("*.csv")), 2):
        if left and right:
            graph[left].add(right)
            graph[right].add(left)
    people = {}
    for node in graph:
        if node in people:
            continue
        component = set()
        todo = [node]
        while todo:
            current = todo.pop()
            if current not in component:
                component.add(current)
                todo.extend(graph[current] - component)
        name = min(component)
        people.update((member, name) for member in component)

    earliest = {}
    for row in read_rows(sorted((data / "events").glob("*.csv")), 7):
        event_id = row[0]
        if event_id and (event_id not in earliest or row[3] < earliest[event_id][3]):
            earliest[event_id] = row
    events = defaultdict(list)
    for _, user, raw_time, raw_ingest, kind, product, price in earliest.values():
        time = timestamp(raw_time)
        if not user or not product or time is None or not start <= time < end:
            continue
        if kind not in (b"view", b"cart", b"purchase"):
            continue
        revenue = 0
        if kind == b"purchase":
            if not DECIMAL.fullmatch(price):
                continue
            significant = price.lstrip(b"0")
            if len(significant) > 8:
                continue
            revenue = int(significant or b"0")
            if not 1 <= revenue <= 10_000_000:
                continue
        if timestamp(raw_ingest) - time > DAY:
            continue
        person = people.get(user, user)
        events[person].append((time, revenue, products.get(product, b"unknown")))

    click_copies = {}
    for row in read_rows(sorted((data / "clicks").glob("*.csv")), 4):
        if row[0]:
            click_copies[row[0]] = row
    clicks = defaultdict(list)
    for click_id, user, campaign, raw_time in click_copies.values():
        time = timestamp(raw_time)
        if user and campaign in campaigns and time is not None and start - WEEK <= time < end:
            clicks[people.get(user, user)].append((time, click_id, campaign))

    users = {}
    campaign_rows = defaultdict(lambda: [0, 0, 0, 0])
    for person, person_events in events.items():
        person_events.sort()
        sessions = []
        previous = None
        for event in person_events:
            if previous is None or event[0] - previous > 1_800_000:
                sessions.append([])
            sessions[-1].append(event)
            previous = event[0]
        for session in sessions:
            session_start = session[0][0]
            day = (EPOCH + timedelta(milliseconds=session_start)).date().isoformat()
            row = users.setdefault((person, day), [0, 0, 0, defaultdict(int)])
            revenue = sum(event[1] for event in session)
            row[0] += 1
            row[1] += len(session)
            row[2] += revenue
            for _, price, category in session:
                if price:
                    row[3][category] += price
            eligible = [click for click in clicks[person] if session_start - WEEK <= click[0] <= session_start]
            campaign = min(eligible, key=lambda click: (-click[0], click[1]))[2] if eligible else b"organic"
            credited = campaign_rows[campaign, day]
            credited[0] += 1
            credited[1] += int(revenue > 0)
            credited[2] += revenue
            for campaign in {click[2] for click in eligible}:
                campaign_rows[campaign, day][3] += 1
    user_output = []
    for (person, day), (sessions, count, revenue, categories) in sorted(users.items()):
        top = min(categories, key=lambda category: (-categories[category], category)) if categories else None
        user_output.append([person, day, sessions, count, revenue, top])
    campaign_output = []
    for (campaign, day), counts in sorted(campaign_rows.items()):
        campaign_output.append([campaign, campaigns.get(campaign, b"organic"), day, *counts])
    return user_output, campaign_output


def main():
    data, output = map(Path, sys.argv[1:])
    output.mkdir(parents=True, exist_ok=True)
    users, campaigns = solve(data)
    tables = [
        ("user_daily", ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"], users),
        ("campaign_daily", ["campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"], campaigns),
    ]
    for name, columns, rows in tables:
        with (output / (name + ".csv")).open("w", encoding="utf-8", newline="") as file:
            writer = csv.writer(file, lineterminator="\n")
            writer.writerow(columns)
            writer.writerows([[value.decode("utf-8") if isinstance(value, bytes) else value for value in row] for row in rows])
        print(f"{name} expected rows {len(rows)}")


if __name__ == "__main__":
    main()
