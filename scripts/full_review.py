#!/usr/bin/env python3
"""Full project review — check all integration points."""
import os, re

BASE = r'E:\dev\openfang'
issues = []
ok = []

def check(desc, cond):
    if cond: ok.append(desc)
    else: issues.append(desc)

# ── Module Registration ──
for mod_name in ['nav_control', 'sensor_fusion', 'weapon_interface', 'report_queue',
                 'direct_channel', 'comm_monitor', 'platform_tools']:
    c = open(f'{BASE}/crates/openfang-runtime/src/lib.rs', 'r', encoding='utf-8').read()
    check(f'Runtime: pub mod {mod_name}', f'pub mod {mod_name};' in c)

for mod_name in ['self_destruct']:
    c = open(f'{BASE}/crates/openfang-kernel/src/lib.rs', 'r', encoding='utf-8').read()
    check(f'Kernel: pub mod {mod_name}', f'pub mod {mod_name};' in c)

for mod_name in ['platform']:
    c = open(f'{BASE}/crates/openfang-types/src/lib.rs', 'r', encoding='utf-8').read()
    check(f'Types: pub mod {mod_name}', f'pub mod {mod_name};' in c)

# ── Workspace Members ──
ws = open(f'{BASE}/Cargo.toml', 'r', encoding='utf-8').read()
check('Workspace: openfang-platform', 'openfang-platform' in ws)
check('Workspace: openfang-platform-arksim', 'openfang-platform-arksim' in ws)

# ── Dependencies ──
rt_cargo = open(f'{BASE}/crates/openfang-runtime/Cargo.toml', 'r', encoding='utf-8').read()
check('Runtime deps: rusqlite', 'rusqlite' in rt_cargo)
check('Runtime deps: sha2', 'sha2' in rt_cargo)
check('Runtime deps: hex', 'hex' in rt_cargo)

kern_cargo = open(f'{BASE}/crates/openfang-kernel/Cargo.toml', 'r', encoding='utf-8').read()
check('Kernel deps: openfang-platform', 'openfang-platform' in kern_cargo)

# ── Kernel Integration ──
kern = open(f'{BASE}/crates/openfang-kernel/src/kernel.rs', 'r', encoding='utf-8').read()
check('Kernel: use openfang_platform', 'use openfang_platform' in kern)
check('Kernel: AdapterRegistry field', 'platform_registry: AdapterRegistry' in kern)
check('Kernel: AdapterRegistry::new()', 'AdapterRegistry::new()' in kern)

# ── Capability Variants ──
cap = open(f'{BASE}/crates/openfang-types/src/capability.rs', 'r', encoding='utf-8').read()
for v in ['WeaponArm', 'WeaponLaunch', 'WeaponAbort', 'PayloadControl', 'SelfDestruct']:
    check(f'Capability: {v}', v in cap)
check('Capability: test_weapon_capability_inheritance', 'test_weapon_capability_inheritance' in cap)

# ── Approval Quorum ──
appr = open(f'{BASE}/crates/openfang-kernel/src/approval.rs', 'r', encoding='utf-8').read()
for method in ['request_approval_quorum', 'add_signature', 'QuorumStatus', 'required_signers']:
    check(f'Approval: {method}', method in appr)

# ── SelfDestructGuard ──
sd = open(f'{BASE}/crates/openfang-kernel/src/self_destruct.rs', 'r', encoding='utf-8').read()
check('SelfDestruct: hmactest', '#[cfg(test)]' in sd)
check('SelfDestruct: verify method', 'fn verify' in sd)

# ── Proto File ──
proto = open(f'{BASE}/protobuf/afsimproto.proto', 'r', encoding='utf-8').read()
for field in ['locationLLA', 'orientationNED', 'velocityNED', 'Weapons', 'SensorStates']:
    check(f'Proto v2: {field}', field in proto)

# ── State Mapper (v2 fields) ──
sm = open(f'{BASE}/crates/openfang-platform-arksim/src/state_mapper.rs', 'r', encoding='utf-8').read()
for field in ['location_lla', 'orientation_ned', 'velocity_ned', 'sensor_states', 'fuel_available']:
    check(f'StateMapper: {field}', field in sm)

# ── Agent System Prompts ──
for agent in ['tca', 'sma', 'na', 'fca', 'ca', 'fma', 'hma', 'ora']:
    path = f'{BASE}/agents/{agent}/SYSTEM_PROMPT.md'
    check(f'Agent prompt: {agent}', os.path.exists(path))

# ── Workflow Definitions ──
wf_path = f'{BASE}/agents/workflows/tactical_workflows.toml'
wf = open(wf_path, 'r', encoding='utf-8').read() if os.path.exists(wf_path) else ''
check('Workflows: file exists', os.path.exists(wf_path))
for wf_name in ['Patrol', 'Track', 'Engage', 'Survive', 'Scuttle',
                'CoordinatedStrike', 'ReconToStrike', 'FleetRecovery',
                'AutoTaskReallocate', 'CommRelayHandoff']:
    check(f'Workflow: {wf_name}', f'name = "{wf_name}"' in wf)

# ── Source Files Exist ──
files = [
    'crates/openfang-platform/src/lib.rs',
    'crates/openfang-platform/src/registry.rs',
    'crates/openfang-platform/src/error.rs',
    'crates/openfang-platform-arksim/src/lib.rs',
    'crates/openfang-platform-arksim/src/bridge.rs',
    'crates/openfang-platform-arksim/src/state_mapper.rs',
    'crates/openfang-platform-arksim/src/command_mapper.rs',
    'crates/openfang-platform-arksim/src/gen/mod.rs',
    'crates/openfang-types/src/platform.rs',
    'crates/openfang-runtime/src/nav_control.rs',
    'crates/openfang-runtime/src/sensor_fusion.rs',
    'crates/openfang-runtime/src/weapon_interface.rs',
    'crates/openfang-runtime/src/report_queue.rs',
    'crates/openfang-runtime/src/direct_channel.rs',
    'crates/openfang-runtime/src/comm_monitor.rs',
    'crates/openfang-runtime/src/platform_tools.rs',
    'crates/openfang-kernel/src/self_destruct.rs',
    'crates/openfang-memory/src/migration.rs',
]
for f in files:
    check(f'File exists: {f}', os.path.exists(f'{BASE}/{f}'))

# ── Summary ──
print(f'\n{"="*60}')
print(f'REVIEW RESULTS: {len(ok)} OK, {len(issues)} ISSUES')
print(f'{"="*60}')
if issues:
    print('\nISSUES:')
    for i in issues:
        print(f'  [FAIL] {i}')
else:
    print('\nAll checks passed.')
