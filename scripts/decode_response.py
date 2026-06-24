import socket, struct, sys
sys.path.insert(0, r'E:\dev\openfang\scripts\gen')
import afsimproto_pb2 as af
import AfsimActionsProto_pb2 as act

sock = socket.socket(); sock.settimeout(5); sock.connect(('127.0.0.1', 18000))
hs = b''; 
while len(hs) < 10: hs += sock.recv(10 - len(hs))

# Send SetOutsideControl
a = act.ActionsFromOutside(); c = a.a_agentcontrl.add(); c.action = 0; c.agent_id = 'Flight_01'
sock.sendall(struct.pack('<I', len(a.SerializeToString())) + a.SerializeToString())

# Read ALL response data
import time; time.sleep(1); sock.settimeout(3)
raw = b''
while True:
    try:
        d = sock.recv(65536)
        if d: raw += d
    except socket.timeout: break
sock.close()

print(f'Total: {len(raw)}B')
print(f'First 64B hex: {raw[:64].hex()}')

# Check if data starts with length prefix
offset = 0
frames = 0
while offset + 4 <= len(raw):
    plen = struct.unpack('<I', raw[offset:offset+4])[0]
    print(f'\nOffset {offset}: plen(LE)={plen}')
    if plen < 1 or plen > 5000000:
        print(f'  Bad frame length, trying from next byte')
        offset += 1
        continue
    if offset + 4 + plen > len(raw):
        print(f'  Incomplete frame (need {plen}B, have {len(raw)-offset-4}B)')
        break
    payload = raw[offset+4:offset+4+plen]
    
    # Try StateMessage
    try:
        msg = af.StateMessage(); msg.ParseFromString(payload)
        print(f'  StateMessage: {len(msg.platforms)} platforms, t={msg.time:.1f}s')
        frames += 1
    except:
        # Try PlatformState
        try:
            p = af.PlatformState(); p.ParseFromString(payload)
            print(f'  PlatformState: [{p.side}] {p.name}')
            frames += 1
        except:
            # Show raw
            print(f'  Parse failed. First 32B: {payload[:32].hex()}')
            # Manual decode
            if payload[0] == 0x0a:  # field 1 LEN-delimited
                dlen = payload[1]
                inner = payload[2:2+dlen]
                print(f'  Field 1: {dlen}B inner')
                # Try PlatformState on inner
                try:
                    p = af.PlatformState(); p.ParseFromString(inner)
                    print(f'  -> PlatformState: [{p.side}] {p.name}')
                except:
                    print(f'  -> Inner also failed: {inner[:32].hex()}')
    
    offset += 4 + plen

print(f'\nTotal frames parsed: {frames}')
