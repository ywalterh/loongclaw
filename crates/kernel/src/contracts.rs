// Re-export all contract types from loongclaw-contracts
pub use loongclaw_contracts::{
    Capability, CapabilityToken, ConnectorCommand, ConnectorOutcome, ExecutionRoute, Fault,
    HarnessKind, HarnessOutcome, HarnessRequest, Namespace, TaskIntent, TaskState,
};

pub use loongclaw_contracts::{
    EvolutionBreadcrumb, EvolutionConfig, EvolutionDecision, EvolutionIssue, EvolutionIssueKind,
    EvolutionMode, EvolutionPhase, EvolutionReport, EvolutionSessionSummary, EvolutionTrigger,
    IssueSeverity, ShellOutput,
};
