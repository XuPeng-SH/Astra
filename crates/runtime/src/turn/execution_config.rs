//! Resolve execution settings from captured admission inputs.
//!
//! Consumers receive the resolved values; resolution never reads ambient state.

pub(crate) fn resolve_context_budget(
    runtime: &astra_config::runtime_config::RuntimeConfig,
    context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
    compact: astra_turn_types::context_execution::CompactConfig,
) -> astra_turn_types::context_execution::ContextBudget {
    astra_turn_types::context_execution::ContextBudget::resolve(
        context_window,
        max_completion_tokens,
        runtime.compression.compression_threshold,
        runtime.compression.preserve_recent_turns as usize,
        (runtime.memory.max_memory_tokens as usize).saturating_mul(4),
        compact,
    )
}
