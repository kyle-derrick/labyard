# labyard Makefile
#
# 顺序很重要：`web` 必须在 `build` 之前，因为 src/web 用 rust-embed 在编译期
# 读取 web/dist/。目录不存在时 cargo build 会直接失败。
#
#   make            → 前端 + 后端
#   make dev        → 只跑前端 dev server（代理到 8443）
#   make check      → 快速类型检查，不产出二进制
#   make test       → 单元测试
#   make cross      → 交叉编译到 amd64（需要 docker；用 cross）

NAME    := labyard
TARGET  := x86_64-unknown-linux-gnu
CARGO   ?= cargo

.PHONY: all web build check test fmt clippy cross clean run install

all: web build

web:
	@if [ ! -d web/node_modules ]; then \
		echo ">> 安装前端依赖"; \
		cd web && npm ci --no-audit --no-fund; \
	fi
	@echo ">> 构建前端"
	cd web && npm run build

build: 
	@echo ">> 构建后端"
	$(CARGO) build --release --locked

check:
	$(CARGO) check --all-targets --locked

test:
	$(CARGO) test --locked

fmt:
	$(CARGO) fmt --all
	$(CARGO) clippy --all-targets -- -D warnings

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

# 交叉编译：rusqlite 要过 C 编译器，所以必须用 cross（Docker 里有工具链）
cross:
	cross build --release --locked --target $(TARGET)
	@echo ">> 产物： target/$(TARGET)/release/$(NAME)"

dev:
	cd web && npm run dev

run:
	$(CARGO) run -- serve --config ./dev-config.toml --port 8443

clean:
	$(CARGO) clean
	rm -rf web/dist web/node_modules
