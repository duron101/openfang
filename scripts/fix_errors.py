import sys, os
base = r'E:\dev\openfang\crates\openfang-runtime\src'

# Fix 1: FireAuth Eq
f = open(f'{base}/weapon_interface.rs','r',encoding='utf-8').read()
f = f.replace('#[derive(Debug, Clone, PartialEq, Eq)]', '#[derive(Debug, Clone, PartialEq)]')
open(f'{base}/weapon_interface.rs','w',encoding='utf-8').write(f)
print('Fixed FireAuth Eq')

# Fix 2: direct_channel mut ref
f = open(f'{base}/direct_channel.rs','r',encoding='utf-8').read()
f = f.replace('pub fn set_rule_enabled(&self,', 'pub fn set_rule_enabled(&mut self,')
open(f'{base}/direct_channel.rs','w',encoding='utf-8').write(f)
print('Fixed direct_channel')

# Fix 3: sensor_fusion double borrow
f = open(f'{base}/sensor_fusion.rs','r',encoding='utf-8').read()
# Restructure: get track_id first, then update separately
old = '''            match correlated {
                Some(track_id) => {
                    // Update existing track
                    if let Some(track) = self.tracks.get_mut(&track_id) {
                        self.kalman_update(track, raw, now);
                    }
                }'''
new = '''            match correlated {
                Some(track_id) => {
                    // Update existing track
                    let entry = self.tracks.get_mut(&track_id);
                    if let Some(track) = entry {
                        kalman_update(track, raw, now);
                    }
                }'''
f = f.replace(old, new)
# Make kalman_update a free function
f = f.replace('    fn kalman_update(&mut self, track: &mut FusedTrack, raw: &Track, now: f64) {',
              'fn kalman_update(track: &mut FusedTrack, raw: &Track, now: f64) {')
# Remove self references in kalman_update
f = f.replace('self.measurement_noise', 'MEASUREMENT_NOISE')
f = f.replace('self.process_noise', 'PROCESS_NOISE')
# Add constants
if 'const MEASUREMENT_NOISE' not in f:
    consts = '\nconst MEASUREMENT_NOISE: f64 = 10.0;\nconst PROCESS_NOISE: f64 = 1.0;\n'
    f = f.replace('const MEASUREMENT_NOISE', '')
    f = f.replace('const PROCESS_NOISE', '')
    f = f.replace('pub struct SensorFusion {', consts + 'pub struct SensorFusion {')

open(f'{base}/sensor_fusion.rs','w',encoding='utf-8').write(f)
print('Fixed sensor_fusion double borrow')
