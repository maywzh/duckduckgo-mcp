.PHONY: dev build release image run stop clean lint test

# Variables
IMAGE_NAME := duckduckgo-mcp
IMAGE_TAG := latest
CONTAINER_NAME := duckduckgo-mcp

# Development - run locally with cargo
dev:
	cargo run

# Build debug binary
build:
	cargo build

# Build release binary
release:
	cargo build --release --locked

# Build Docker image
image:
	docker build -t $(IMAGE_NAME):$(IMAGE_TAG) .

# Run with docker-compose
run:
	docker-compose up -d

# Stop docker-compose services
stop:
	docker-compose down

# Rebuild and run (stop, rebuild image, start)
rebuild: stop image run

# Clean build artifacts
clean:
	cargo clean
	docker-compose down -v 2>/dev/null || true

# Lint code
lint:
	cargo clippy -- -D warnings

# Run tests
test:
	cargo test

# Format code
fmt:
	cargo fmt

# Check formatting
fmt-check:
	cargo fmt -- --check

# Shell into container
shell:
	docker exec -it $(CONTAINER_NAME) /bin/sh

# View logs
logs:
	docker-compose logs -f

# Build and tag with version
tag-version:
	docker build -t $(IMAGE_NAME):$(IMAGE_TAG) -t $(IMAGE_NAME):$(shell git rev-parse --short HEAD) .

# Push image to registry (set REGISTRY var)
push:
	docker push $(IMAGE_NAME):$(IMAGE_TAG)
