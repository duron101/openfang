c=open('crates/openfang-runtime/src/sensor_fusion.rs','r',encoding='utf-8').read()
# Close impl block before kalman_update, make it standalone
old_close = '''    fn kalman_update(track: &mut FusedTrack, raw: &Track, now: f64) {'''
new_close = '''}

/// Simple 1D Kalman update for position.
fn kalman_update(track: &mut FusedTrack, raw: &Track, now: f64) {'''
c = c.replace(old_close, new_close)
open('crates/openfang-runtime/src/sensor_fusion.rs','w',encoding='utf-8').write(c)
print('Fixed kalman_update scope')
