#!/usr/bin/env python3
"""Test ArkSIM with ONLY the reference proto v2 — no gen/ pollution."""
import socket, struct, time, sys

# Use ONLY reference implementation proto
sys.path.insert(0, r'E:\dev\alpha\1202\0422\Liu_mid_ui\afsim_py')
import afsimproto_v2_pb2 as af

# We can't import ActionsFromOutside (different proto pool)
# So construct it manually from raw bytes

sock = socket.socket(); sock.settimeout(5)
sock.connect(('127.0.0.1', 18000))
print('Connected')

# Read handshake
hs = b''
while len(hs) < 10: hs += sock.recv(10 - len(hs))
print(f'Handshake: {hs.hex()}')

# Construct SetOutsideControl manually (protobuf raw bytes)
# AgentContrl: action=0 (varint), agent_id="Flight_01" (LEN)
# Field 1 in ActionsFromOutside is a_agentcontrl (LEN)
# AgentContrl: field 1 = action (varint 0), field 2 = agent_id (string "Flight_01")
agent_contrl = bytes([0x08, 0x00, 0x12, 0x0a, 0x46, 0x6c, 0x69, 0x67, 0x68, 0x74, 0x5f, 0x30, 0x31])
# ActionsFromOutside: field 1 = a_agentcontrl (LEN 13)
actions = bytes([0x0a, len(agent_contrl)]) + agent_contrl

print(f'Raw SetOutsideControl: {actions.hex()} ({len(actions)}B)')
sock.sendall(struct.pack('<I', len(actions)) + actions)
print('Sent SetOutsideControl')

# Read response
time.sleep(1)
sock.settimeout(3)
raw = b''
while True:
    try:
        d = sock.recv(65536)
        if d: raw += d
    except socket.timeout: break

print(f'Response: {len(raw)}B')

# Try parsing as StateMessage
msg = af.StateMessage()
try:
    msg.ParseFromString(raw[4:4+255])  # First frame (255 bytes payload)
    print(f'PARSED: {len(msg.platforms)} platforms, t={msg.time}s')
    for p in msg.platforms[:3]:
        print(f'  [{p.side}] {p.name} type={p.type} domain={p.spatialDomain}')
except Exception as e:
    print(f'Parse failed: {e}')
    # Show raw
    print(f'  First 64B: {raw[:64].hex()}')

sock.close()
