# bench —— 语言选型的实测依据

这不是产品代码，是**决策证据**。设计文档 §7.5 里那张 Python / Go / Rust 的内存对比表，
就是用它跑出来的。放在仓库里是为了让结论可复现，而不是"相信我，我算过"。

## 测什么

三份代码做**完全相同**的三件事：

| 端点 | 行为 |
|---|---|
| `GET /health` | 返回 `{"ok":true}` |
| `GET /items?n=N` | 查 SQLite，返回 N 行（每行含 200 字节 payload） |
| `POST /echo` | 校验 JSON body 的 `name`（非空、≤64），回显 |

三份实现各自用最主流的组合，不故意选轻量库：

| | 框架 | 数据库 |
|---|---|---|
| Python | FastAPI + Pydantic + Uvicorn | 标准库 `sqlite3` |
| Go | 标准库 `net/http` + `encoding/json` | `modernc.org/sqlite`（纯 Go） |
| Rust | axum + serde | rusqlite（`bundled`，静态编译 SQLite） |

## 怎么量

`measure.sh` 的过程：

1. 启动进程，轮询 `/health` 直到就绪，记启动耗时
2. 预热 30 次请求
3. 记 `VmRSS`（空闲）
4. `500 请求 / 50 并发` 压测，期间每 150 ms 采样一次 fd 数
5. 记 `VmHWM`（**峰值**，内核维护的高水位，不会被后续释放抹掉）
6. 等 2 秒，再记 `VmRSS`（看有没有归还）

用 `/proc/PID/status` 而不是 `ps`：`VmHWM` 才是真实峰值，`ps` 只能看到瞬时值。

## 怎么复现

在一台 Debian 13 / arm64 的机器或容器里：

```bash
# 依赖
apt-get install -y python3 python3-venv curl
python3 -m venv /opt/bench/py && /opt/bench/py/bin/pip install fastapi uvicorn pydantic

# Go
curl -fsSL "https://go.dev/dl/$(curl -s 'https://go.dev/dl/?mode=json' | python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["version"])').linux-arm64.tar.gz" \
  | tar -C /usr/local -xz
cd go_app && GOPROXY=https://goproxy.cn go build -trimpath -ldflags="-s -w" -o /opt/bench/go_app .

# Rust（国内建议 rsproxy）
export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --profile minimal
cd rs_app && cargo build --release

# 跑（每轮之间删库，保证三边做同样的初始化）
for r in 1 2; do
  rm -f /opt/bench/bench.db*
  bash measure.sh python /opt/bench/py/bin/python ./py_app.py
  rm -f /opt/bench/bench.db*
  bash measure.sh go /opt/bench/go_app
  rm -f /opt/bench/bench.db*
  bash measure.sh rust ./rs_app/target/release/bench
done
```

## 结果（lab3：Debian 13 / arm64）

| 指标 | Python | Go | Rust |
|---|---|---|---|
| 启动 | 15–19 ms | 21–28 ms | 24 ms |
| 空闲 RSS | 34–44 MB | 11.4 MB | 4.7 MB |
| **峰值 RSS** | **56.3–59.2 MB** | **18.3–18.6 MB** | **6.4–6.6 MB** |
| 线程 | 24–33 | 9 | 8 |
| fd 峰值 | 54–68 | 26–29 | 15–16 |
| 二进制 | venv ≈ 90 MB | 9.9 MB | 2.6 MB |

## 两条读数的注意事项

1. **吞吐不要引用这张表。** 同一份代码两轮跑出 767 ms 和 5546 ms，差 7 倍——容器内的
   CPU 调度是主要变量。要测吞吐得换裸机 + 固定频率核，这个实验环境测不出来。
2. **内存两轮一致，可以引用。** Python 空闲值在 34–44 MB 之间抖动（GC 与页回收），
   但峰值稳定在 56–59 MB；Go 与 Rust 两边几乎不动。

## 结论去哪了

看 `docs/design.md` §7.5。一句话：**Python → Go 省 41 MB，Go → Rust 再省 11.7 MB，
但后者要付 2–3 倍开发时间和一个零现成的 PVE 客户端。边际收益已经小于边际成本。**
