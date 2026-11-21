IMAGE ?= shopping-sessions:latest
CPUS ?= 8
MEM ?= 24g
PLATFORM ?= linux/amd64
DATA ?= dev
OUT ?= out
ANSWER ?= dev-answer
export IMAGE CPUS MEM PLATFORM

.PHONY: build fixtures dev run check-dev check-totals test

build:
	docker build --platform $(PLATFORM) -t $(IMAGE) .

fixtures: build
	python3 scripts/run_fixtures.py fixtures -- sh scripts/docker_run.sh

dev:
	generate --seed 1 --events 1000000 --clicks 250000 --output "$(DATA)"

run: build
	sh scripts/docker_run.sh "$(DATA)" "$(OUT)"

check-dev:
	python3 scripts/compare.py "$(OUT)" "$(ANSWER)"

check-totals:
	python3 scripts/check_totals.py "$(OUT)"

test:
	cargo test --release --locked
