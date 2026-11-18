//! 核心域模型：YAML 规格、三层合并、zone 拓扑、步骤计划。
//!
//! 这一层**不 import axum**，也不起子进程——它只做"从文本到动作列表"的纯函数变换。
//! 这样 planner 可以在没有 PVE 的机器上完整单测。

pub mod plan;
pub mod runner;
pub mod spec;

// 不做根级 re-export：全仓统一写 `crate::core::plan::Plan` 这种全路径，
// 一眼能看出东西在哪一层。加了 re-export 反而会攒出一堆"unused import"警告，
// 并且让 `core::Spec` 和 `spec::Spec` 两种写法并存。
