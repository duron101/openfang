import socket, struct, sys, os, time
sys.path.insert(0, r'E:\dev\openfang\scripts\gen')
import afsimproto_pb2 as af
import AfsimActionsProto_pb2 as act

HOST, PORT = "127.0.0.1", 18000

sock = socket.socket(); sock.settimeout(10)
sock.connect((HOST, PORT))
print(f'Connected to {PORT}')

# Read handshake
sock.settimeout(3)
hs = b''
while len(hs) < 10:
    try:
        hs += sock.recv(10 - len(hs))
    except socket.timeout:
        break
print(f'Handshake: {len(hs)}B hex={hs.hex() if hs else "NONE"}')

# Try receiving data in a loop
sock.settimeout(2)
raw = b''
start = time.time()
while time.time() - start < 5.0:
    try:
        d = sock.recv(65536)
        if d: raw += d
    except socket.timeout: break

print(f'Raw data after handshake: {len(raw)}B')

# Try to parse as StateMessage
if len(raw) >= 4:
    plen = struct.unpack('<I', raw[:4])[0]
    print(f'First frame length hint (LE): {plen}')
    if 4 < plen < 10000000 and len(raw) >= 4 + plen:
        payload = raw[4:4+plen]
        msg = af.StateMessage()
        try:
            msg.ParseFromString(payload)
            print(f'PARSED: {len(msg.platforms)} platforms, t={msg.time:.1f}s')
            if msg.platforms:
                p = msg.platforms[0]
                print(f'  [{p.side}] {p.name}')
        except Exception as e:
            print(f'Parse failed: {e}')
            # Try as PlatformState directly
            try:
                p = af.PlatformState()
                p.ParseFromString(payload)
                print(f'PARSED as PlatformState: {p.name}')
            except Exception as e2:
                print(f'PlatformState parse also failed: {e2}')

# Try sending SetOutsideControl and see if that triggers data
print('\nSending SetOutsideControl for Flight_01...')
a = act.ActionsFromOutside()
c = a.a_agentcontrl.add(); c.action = 0; c.agent_id = 'Flight_01'
p = a.SerializeToString()
sock.sendall(struct.pack('<I', len(p)) + p)
print(f'Sent {len(p)}B')

# Read response
time.sleep(1)
sock.settimeout(3)
raw2 = b''
while True:
    try:
        d = sock.recv(65536)
        if d: raw2 += d
    except socket.timeout: break

print(f'Response: {len(raw2)}B')
if len(raw2) >= 4:
    plen = struct.unpack('<I', raw2[:4])[0]
    print(f'Response frame length: {plen}')
    if 4 < plen < 10000000:
        try:
            msg = af.StateMessage()
            msg.ParseFromString(raw2[4:4+plen])
            print(f'PARSED: {len(msg.platforms)} platforms')
            for p in msg.platforms[:3]:
                print(f'  {p.name}: side={p.side} domain={p.spatialDomain}')
        except Exception as e:
            print(f'Response parse failed: {e}')

sock.close()
print('\nDone')
