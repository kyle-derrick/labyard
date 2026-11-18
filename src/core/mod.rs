//! 核心域模型：YAML 规格、三层合并、zone 拓扑、步骤计划。
//!
//! 这一层**不 import axum**，也不起子进程——它只做"从文本到动作列表"的纯函数变换。
//! 这样 planner 可以在没有 PVE 的机器上完整单测。

pub mod plan;
pub mod runner;
pub mod spec;

pub use plan::{Phase, Plan, Step, StepKind};
pub use spec::{ResolvedHost, ResolvedZone, Role, Spec, SpecError, V6Mode};
