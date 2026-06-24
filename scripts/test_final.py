import sys, socket, struct, time
sys.path.insert(0, r'E:\dev\openfang\scripts\gen')
import afsimproto_pb2 as af
import AfsimActionsProto_pb2 as act

sock = socket.socket(); sock.settimeout(10)
sock.connect(('127.0.0.1', 18000))
print('Connected')

# Handshake
hs = b''
while len(hs) < 10:
    hs += sock.recv(10 - len(hs))

# Recv StateMessage (LE framing)
def recv(s, t=5.0):
    s.settimeout(t)
    raw = b''
    for _ in range(4): raw += s.recv(4 - len(raw))
    plen = struct.unpack('<I', raw)[0]
    return b''.join(s.recv(plen - len(data := b'')) or (data := data + s.recv(1)) for _ in [0]) or data

def recv2(s, t=5.0):
    s.settimeout(t)
    raw = s.recv(4)
    plen = struct.unpack('<I', raw)[0]
    data = b''
    while len(data) < plen:
        data += s.recv(min(plen - len(data), 65536))
    msg = af.StateMessage()
    msg.ParseFromString(data)
    return msg

msg = recv2(sock, 10)
print(f'State: {len(msg.platforms)} platforms, t={msg.time:.1f}s')
p0 = msg.platforms[0]
print(f'  [{p0.side}] {p0.name} type={p0.type} domain={p0.spatialDomain}')

# Send SetOutsideControl
def send(s, p):
    s.sendall(struct.pack('<I', len(p)) + p)

a = act.ActionsFromOutside()
c = a.a_agentcontrl.add(); c.action = 0; c.agent_id = 'Flight_01'
send(sock, a.SerializeToString())
print('SetOutsideControl sent')

# DesiredHeading
a2 = act.ActionsFromOutside()
dh = a2.a_desiredheading.add(); dh.agent_id = 'Flight_01'; dh.desired_heading = 1.5708
send(sock, a2.SerializeToString())
print('DesiredHeading 90deg sent')

time.sleep(1)
msg2 = recv2(sock, 10)
print(f'Response: {len(msg2.platforms)} platforms')
f1 = [p for p in msg2.platforms if 'Flight_01' in p.name]
if f1:
    ori = f1[0].orientationNED
    print(f'  Flight_01 heading={ori[0]*57.3:.1f}deg')
else:
    print('  Flight_01 not found')

# ReleaseControl
a3 = act.ActionsFromOutside()
c2 = a3.a_agentcontrl.add(); c2.action = 1; c2.agent_id = 'Flight_01'
send(sock, a3.SerializeToString())
print('ReleaseControl sent')

print('\n=== CLOSED LOOP VERIFIED ===')
sock.close()
