# Makefile for common tasks in a Rust project
# Detect current branch
CURRENT_BRANCH := $(shell git rev-parse --abbrev-ref HEAD)
ZIP_NAME = OptionChain-Simulator.zip
PROJECT_NAME := optionchain-simulator

# Container image coordinates. VERSION is read from the [package] table of
# Cargo.toml (the versions in [workspace.dependencies] are skipped), so the
# published tag can never drift from the crate version.
REGISTRY ?= ghcr.io
IMAGE_NAME ?= $(REGISTRY)/joaquinbejar/optionchain-simulator
VERSION := $(shell awk '/^\[package\]/ { in_pkg = 1; next } /^\[/ { in_pkg = 0 } in_pkg && /^version[[:space:]]*=/ { gsub(/[",]/, "", $$3); print $$3; exit }' Cargo.toml)

# Build the release image for the version in Cargo.toml.
.PHONY: docker-build
docker-build:
	@test -n "$(VERSION)" || (echo "could not read [package] version from Cargo.toml"; exit 1)
	docker build -f Docker/Dockerfile -t $(IMAGE_NAME):$(VERSION) -t $(IMAGE_NAME):latest .

# Build and push that image to the registry. Requires a prior
# `docker login $(REGISTRY)` with a token carrying write:packages.
.PHONY: docker-push
docker-push: docker-build
	docker push $(IMAGE_NAME):$(VERSION)
	docker push $(IMAGE_NAME):latest
	@echo "pushed $(IMAGE_NAME):$(VERSION) and :latest"

# The stack network is external and shared with other stacks, so it is created
# once per environment instead of by the compose file: bridge on a local
# engine, attachable overlay on a swarm (the only scope swarm accepts for
# services). Both targets are idempotent.
OPTIONCHAIN_NETWORK ?= optionchain-network

.PHONY: network
network:
	@docker network inspect $(OPTIONCHAIN_NETWORK) >/dev/null 2>&1 \
		|| docker network create --driver bridge $(OPTIONCHAIN_NETWORK)

.PHONY: network-swarm
network-swarm:
	@docker network inspect $(OPTIONCHAIN_NETWORK) >/dev/null 2>&1 \
		|| docker network create --driver overlay --attachable $(OPTIONCHAIN_NETWORK)

# Local stack: the deployable services plus the dev override (admin UIs and
# host-published infrastructure ports).
.PHONY: deploy
deploy: network
	OPTIONCHAIN_VERSION=$(VERSION) OPTIONCHAIN_NETWORK=$(OPTIONCHAIN_NETWORK) \
		docker compose -p $(PROJECT_NAME) \
		-f Docker/docker-compose.yml -f Docker/docker-compose.dev.yml \
		up --pull always --force-recreate -d

# Same stack on a swarm manager: overlay network, no build context, and never
# the dev override (admin UIs with default credentials, published infra ports).
.PHONY: deploy-swarm
deploy-swarm: network-swarm
	OPTIONCHAIN_VERSION=$(VERSION) OPTIONCHAIN_NETWORK=$(OPTIONCHAIN_NETWORK) \
		docker stack deploy --with-registry-auth -c Docker/docker-compose.yml $(PROJECT_NAME)

# Default target
.PHONY: all
all: test fmt lint build

# Build the project
.PHONY: build
build:
	cargo build

# Features the release binary carries, and therefore what the published image
# serves (Docker/Dockerfile builds through this target).
#
# `arrow-export` is off by default in Cargo.toml because a library consumer
# should not pay for the arrow dependency to use the crate. A DEPLOYMENT is the
# opposite case: the OpenAPI document it serves advertises `format=arrow`, so an
# image without the feature refuses a format its own contract offers (issue
# #148). Override with `make release RELEASE_FEATURES=` to build without it.
RELEASE_FEATURES ?= arrow-export

.PHONY: release
release:
	cargo build --release $(if $(RELEASE_FEATURES),--features "$(RELEASE_FEATURES)",)

# Run tests
.PHONY: test
test:
	LOGLEVEL=WARN cargo test

# Run the integration tests against a DEPLOYED service.
#
# OCS_INTEGRATION_BASE_URL names it, scheme and port included. Without it every
# test skips and says so, which is why this target is safe to run anywhere but
# proves nothing until the variable is set. Where a deployment lives is an
# operator's business and is never recorded in this repository.
.PHONY: test-integration
test-integration:
	@test -n "$$OCS_INTEGRATION_BASE_URL" || \
		echo "OCS_INTEGRATION_BASE_URL is unset: every integration test will skip"
	LOGLEVEL=WARN cargo test -p examples_integration --all-features -- --nocapture

# Prove an existing MongoDB deployment survives the image this repo pins.
#
# Starts the previous image on a volume, writes a document, swaps the binary
# underneath, and requires the document and the FCV to survive before raising
# it. Needs docker and pulls two database images, so it is not part of
# `pre-push`; CI runs it when the MongoDB pin moves.
.PHONY: test-mongo-upgrade
test-mongo-upgrade:
	./scripts/mongo_upgrade_test.sh

# Format the code
.PHONY: fmt
fmt:
	cargo +stable fmt --all

# Check formatting
.PHONY: fmt-check
fmt-check:
	cargo +stable fmt --check

# Run Clippy for linting
.PHONY: lint
lint:
	cargo clippy --all-targets --all-features -- -D warnings

.PHONY: lint-fix
lint-fix: 
	cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings

# Clean the project
.PHONY: clean
clean:
	cargo clean

# Pre-push checks
.PHONY: check
check: test fmt-check lint

# Run the project
.PHONY: run
run:
	cargo run

.PHONY: fix
fix:
	cargo fix --allow-staged --allow-dirty

.PHONY: pre-push
pre-push: fix fmt lint-fix test readme doc

.PHONY: doc
doc:
	cargo clippy -- -W missing-docs

.PHONY: doc-open
doc-open:
	cargo doc --open

.PHONY: publish
publish: readme
	cargo login ${CARGO_REGISTRY_TOKEN}
	cargo package
	cargo publish

.PHONY: coverage
coverage:
	export LOGLEVEL=WARN
	cargo install cargo-tarpaulin
	mkdir -p coverage
	cargo tarpaulin --verbose --all-features --workspace --timeout 120 --out Xml

.PHONY: coverage-html
coverage-html:
	export LOGLEVEL=WARN
	cargo install cargo-tarpaulin
	mkdir -p coverage
	cargo tarpaulin --verbose --all-features --workspace --timeout 120 --out Html

.PHONY: open-coverage
open-coverage:
	open tarpaulin-report.html

# Rule to show git log
git-log:
	@if [ "$(CURRENT_BRANCH)" = "HEAD" ]; then \
		echo "You are in a detached HEAD state. Please check out a branch."; \
		exit 1; \
	fi; \
	echo "Showing git log for branch $(CURRENT_BRANCH) against main:"; \
	git log main..$(CURRENT_BRANCH) --pretty=full

.PHONY: create-doc
create-doc:
	cargo doc --no-deps --document-private-items

.PHONY: readme
readme: check-cargo-readme create-doc
	cargo readme > README.md

.PHONY: check-cargo-readme
check-cargo-readme:
	@command -v cargo-readme > /dev/null || (echo "Installing cargo-readme..."; cargo install cargo-readme)

.PHONY: check-spanish
check-spanish:
	cd scripts && python3 spanish.py ../src && cd ..

.PHONY: zip
zip:
	@echo "Creating $(ZIP_NAME) without any 'target' directories, 'Cargo.lock', and hidden files..."
	@find . -type f \
		! -path "*/target/*" \
		! -path "./.*" \
		! -name "Cargo.lock" \
		! -name ".*" \
		| zip -@ $(ZIP_NAME)
	@echo "$(ZIP_NAME) created successfully."


.PHONY: check-cargo-criterion
check-cargo-criterion:
	@command -v cargo-criterion > /dev/null || (echo "Installing cargo-criterion..."; cargo install cargo-criterion)

.PHONY: bench
bench: check-cargo-criterion
	cargo criterion --output-format=quiet

.PHONY: bench-show
bench-show:
	open target/criterion/report/index.html

.PHONY: bench-save
bench-save: check-cargo-criterion
	cargo criterion --output-format quiet --history-id v0.3.2 --history-description "Version 0.3.2 baseline"

.PHONY: bench-compare
bench-compare: check-cargo-criterion
	cargo criterion --output-format verbose

.PHONY: bench-json
bench-json: check-cargo-criterion
	cargo criterion --message-format json

.PHONY: bench-clean
bench-clean:
	rm -rf target/criterion


.PHONY: workflow-coverage
workflow-coverage:
	DOCKER_HOST="$${DOCKER_HOST}" act push --job code_coverage_report \
       -P ubuntu-latest=catthehacker/ubuntu:latest \
       --privileged

.PHONY: workflow-build
workflow-build:
	DOCKER_HOST="$${DOCKER_HOST}" act push --job build \
       -P ubuntu-latest=catthehacker/ubuntu:latest

.PHONY: workflow-lint
workflow-lint:
	DOCKER_HOST="$${DOCKER_HOST}" act push --job lint

.PHONY: workflow-test
workflow-test:
	DOCKER_HOST="$${DOCKER_HOST}" act push --job run_tests

.PHONY: workflow
workflow: workflow-build workflow-lint workflow-test workflow-coverage
