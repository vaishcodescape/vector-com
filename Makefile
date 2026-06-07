# =======================================
# Vectorcom — two commands, all UI in the Rust TUI
#
#   make server   build + run the C++ backend AND the Rust dashboard TUI
#   make client   build + run the Rust client TUI
#
# Override host/port/user on the command line, e.g.:
#   make server PORT=9090
#   make client HOST=127.0.0.1 PORT=9090 USER=admin
# =======================================

CXX = g++
CXXFLAGS = -std=c++17 -Wall -Wextra -O2 -pthread

SERVER_BIN = vector-com
SERVER_SRC = cpp-server/vector-com.cpp
RUST_DIR   = rust-tui

HOST ?= 127.0.0.1
PORT ?= 8080
USER ?= $(shell id -un)

# -------------------------------
# server: C++ backend (headless) + Rust observer dashboard, one command.
# The backend runs in the background; the dashboard attaches to it as the UI.
# When the dashboard exits, the backend is stopped.
# -------------------------------
server:
	$(CXX) $(CXXFLAGS) -o $(SERVER_BIN) $(SERVER_SRC)
	cd $(RUST_DIR) && cargo build --release
	@echo "Starting backend on port $(PORT)..."
	@./$(SERVER_BIN) $(PORT) & echo $$! > .server.pid
	@sleep 1
	-cd $(RUST_DIR) && cargo run --release -p vectorcom-server -- --observe $(HOST):$(PORT)
	@kill `cat .server.pid` 2>/dev/null || true
	@rm -f .server.pid

# -------------------------------
# client: Rust TUI client (connects to the backend over the XOR protocol).
# -------------------------------
client:
	cd $(RUST_DIR) && cargo build --release
	cd $(RUST_DIR) && cargo run --release -p vectorcom-client -- --host $(HOST) --port $(PORT) -u $(USER)

.PHONY: server client
