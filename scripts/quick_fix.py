c=open('crates/openfang-runtime/src/sensor_fusion.rs','r',encoding='utf-8').read()
c=c.replace('kalman_update(track, raw, now);','Self::kalman_update(track, raw, now);')
open('crates/openfang-runtime/src/sensor_fusion.rs','w',encoding='utf-8').write(c)
print('Fixed')
