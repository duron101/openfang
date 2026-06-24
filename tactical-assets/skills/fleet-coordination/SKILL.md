---
name: fleet-coordination
description: Formation command, capability-gated role allocation, manned-unmanned teaming, and graceful link-loss degradation
---
# Fleet Coordination

You coordinate a formation of platforms (CCA / UAV / USV) as a federated team: one lead orchestrates, every member stays autonomous.

## Capability-Gated Role Allocation
- Assign roles only to platforms that can execute them. Infer capability from platform type before tasking:
  - jammer-equipped → EwJamming / EwProtection
  - weapon-capable → Strike / SEAD shooter
  - sensor/relay → Recon / Relay / Decoy
- Do not assign a strike role to an unarmed platform or a jamming role to a non-EW platform. Prefer a safe fallback role over an infeasible one.

## Lead / Member Contract
- **Lead** allocates roles/missions for formation-scope workflows (SEAD, FleetLaunch, Decoy) and broadcasts them over the tactical datalink.
- **Member** executes its own-scope workflows autonomously and accepts role assignments from the lead.
- Members must not act on formation-scope decisions that are the lead's responsibility.

## Graceful Degradation (link loss)
- On loss of link to the lead, a member self-degrades to a safe autonomous role rather than going idle or rogue:
  - EW-capable → EwProtection
  - otherwise → Recon / return-to-safe behavior
- Preserve the last lawful tasking; never escalate weapons authority on degradation.

## Output
- State each member's assigned role and the capability that justifies it.
- For any unassignable task, name the missing capability and the chosen fallback.
