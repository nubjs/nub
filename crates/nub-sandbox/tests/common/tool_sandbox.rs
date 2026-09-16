use nub_sandbox::{Degradation, Sandbox, SandboxPolicy};

pub fn acquire(policy: &SandboxPolicy) -> Result<Sandbox, Degradation> {
    Sandbox::new(policy)
}
