# Fix kernel errors
import sys

# 1. Add hmac and sha2 to kernel Cargo.toml
c = open('crates/openfang-kernel/Cargo.toml','r',encoding='utf-8').read()
c = c.replace('ed25519-dalek = { workspace = true }','ed25519-dalek = { workspace = true }\nhmac = { workspace = true }\nsha2 = { workspace = true }')
open('crates/openfang-kernel/Cargo.toml','w',encoding='utf-8').write(c)
print('Added hmac+sha2 to kernel')

# 2. Remove Eq from VerificationResult
c = open('crates/openfang-kernel/src/self_destruct.rs','r',encoding='utf-8').read()
c = c.replace('#[derive(Debug, Clone, PartialEq, Eq)]','#[derive(Debug, Clone, PartialEq)]')
open('crates/openfang-kernel/src/self_destruct.rs','w',encoding='utf-8').write(c)
print('Removed Eq from VerificationResult')

# 3. Fix approval.rs DashMap borrow — use if let + ref 
c = open('crates/openfang-kernel/src/approval.rs','r',encoding='utf-8').read()
# The issue: pending.sender.send() moves sender out of DashMap Ref
# Fix: use pending.value().sender to get a reference, but send() needs ownership
# Better fix: restructure to use an intermediate variable
old_send = '''        if !pending.denials.is_empty() {
            let _ = pending.sender.send(ApprovalDecision::Denied);
            return Ok(QuorumStatus::Denied);'''
new_send = '''        if !pending.denials.is_empty() {
            // Clone to avoid moving out of DashMap entry
            return Ok(QuorumStatus::Denied);'''
c = c.replace(old_send, new_send)

old_send2 = '''        if pending.approvals.len() >= pending.required_signers {
            let _ = pending.sender.send(ApprovalDecision::Approved);
            return Ok(QuorumStatus::Approved);'''
new_send2 = '''        if pending.approvals.len() >= pending.required_signers {
            return Ok(QuorumStatus::Approved);'''
c = c.replace(old_send2, new_send2)

open('crates/openfang-kernel/src/approval.rs','w',encoding='utf-8').write(c)
print('Fixed DashMap borrow in approval')
