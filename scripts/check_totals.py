#!/usr/bin/env python3
"""Check schemas, unique keys, and cross-table invariants in a pipeline result."""
from pathlib import Path
import sys

import polars as pl

from run_fixtures import SCHEMAS, load_actual


def main():
    folder = Path(sys.argv[1])
    users = load_actual(folder / "user_daily", SCHEMAS["user_daily"])
    campaigns = load_actual(folder / "campaign_daily", SCHEMAS["campaign_daily"])
    assert users.n_unique(["user_id", "day"]) == users.height, "duplicate person/day"
    assert campaigns.n_unique(["campaign_id", "day"]) == campaigns.height, "duplicate campaign/day"
    for column in ["sessions", "revenue_cents"]:
        assert users[column].sum() == campaigns[column].sum(), f"{column} totals differ"
    nonorganic = campaigns.filter(pl.col("campaign_id") != "organic")
    assert nonorganic["assisted_sessions"].sum() >= nonorganic["sessions"].sum()
    assert campaigns.filter(pl.col("campaign_id") == "organic")["assisted_sessions"].sum() == 0
    assert campaigns.filter(pl.col("purchasing_sessions") > pl.col("sessions")).is_empty()
    assert users.filter(pl.col("events") < pl.col("sessions")).is_empty()
    print(f"PASS user rows {users.height}, campaign rows {campaigns.height}, "
          f"sessions {users['sessions'].sum()}, revenue cents {users['revenue_cents'].sum()}")


if __name__ == "__main__":
    main()
