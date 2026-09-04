# kist — dev entry points. Everything CI runs, you can run here.

BIN     := kist
PKG     := ./cmd/kist
MODULE  := github.com/at-least/kist
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)

# Pinned in lockstep with .github/workflows/ci.yml. Bump both together.
GOLANGCI_VERSION := v2.13.2
GOLANGCI := $(shell command -v golangci-lint 2>/dev/null)

FUZZTIME ?= 30s
## FUZZPARALLEL: fuzz workers per target. Empty means one per core, which
## saturates the machine; set it to a fraction of the cores to keep the
## machine usable while a long campaign runs.
FUZZPARALLEL ?=

.PHONY: build test test-s3 test-sftp test-race vet lint fuzz fuzz-long release-snapshot verify clean

## build: compile the release binary. No cgo, ever — a kist binary must
## run on any machine of its GOOS/GOARCH without a libc to match.
build:
	CGO_ENABLED=0 go build -trimpath \
		-ldflags "-s -w -X $(MODULE)/internal/cmd.version=$(VERSION)" \
		-o $(BIN) $(PKG)

## test: the suite as the shipped binary is built — no cgo.
test:
	CGO_ENABLED=0 go test ./...

## test-sftp: the SFTP backend against OpenSSH in Docker
test-sftp:
	KIST_SFTP_TEST=1 CGO_ENABLED=1 go test -race -count=1 ./internal/backend/ -run 'SFTP'

## test-s3: the S3 backend and the multi-client tests against a MinIO the
## tests start in Docker. Offline by default; this is the one command
## both CI and a developer run for it.
test-s3:
	KIST_S3_TEST=1 CGO_ENABLED=1 go test -race -count=1 ./internal/backend/ ./internal/repo/ -run 'S3|Concurrent|Policy'

## test-race: the same suite under the race detector. -race needs cgo, so
## this is a test-only build and never produces a shipped artifact.
test-race:
	CGO_ENABLED=1 go test -race ./...

vet:
	go vet ./...

## lint: golangci-lint. Prefers whatever binary is on PATH (fast, and what
## you usually want locally); with none, fetches the pinned version. The
## PATH binary is NOT version-checked — .golangci.yml is a v2 config, so a
## v1 binary on PATH will fail here and pass in CI, or the reverse.
lint:
ifeq ($(GOLANGCI),)
	go run github.com/golangci/golangci-lint/v2/cmd/golangci-lint@$(GOLANGCI_VERSION) run ./...
else
	$(GOLANGCI) run ./...
endif

## release-snapshot: every release artifact, built locally from the working
## tree with no tag and no token. This is how the release config is tested.
GORELEASER_VERSION ?= v2.18.0
release-snapshot:
	go run github.com/goreleaser/goreleaser/v2@$(GORELEASER_VERSION) build --snapshot --clean

## fuzz-long: the 24-hour campaign PLAN asks for: 24 hours in total, so
## the per-target budget is 24h divided by the number of targets. Same
## targets, same loop; only the budget differs.
fuzz-long:
	@n=$$(grep -rho --include='*_test.go' -E '^func Fuzz[A-Za-z0-9_]*' . | sort -u | wc -l); \
	each=$$((24 * 60 / n)); \
	echo "$$n targets, $${each}m each"; \
	$(MAKE) fuzz FUZZTIME=$${each}m

## fuzz: run every FuzzXxx target for FUZZTIME each. `go test -fuzz` takes
## exactly one package, so the targets are discovered and run one by one.
fuzz:
	@pkgs=$$(grep -rl --include='*_test.go' -E '^func Fuzz[A-Z_]' . \
		| xargs -r -n1 dirname | sort -u); \
	if [ -z "$$pkgs" ]; then \
		echo "no fuzz targets yet"; exit 0; \
	fi; \
	for p in $$pkgs; do \
		for f in $$(grep -ho -E '^func (Fuzz[A-Za-z0-9_]*)' $$p/*_test.go \
			| sed 's/^func //' | sort -u); do \
			echo "==> $$p $$f ($(FUZZTIME))"; \
			go test "$$p" -run '^$$' -fuzz "^$$f$$" -fuzztime $(FUZZTIME) $(if $(FUZZPARALLEL),-parallel $(FUZZPARALLEL)) || exit 1; \
		done; \
	done

## verify: what a change must pass before it counts as done.
verify: build vet lint test test-race

clean:
	rm -f $(BIN)
	go clean -testcache
