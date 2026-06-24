c = open('crates/openfang-runtime/src/direct_channel.rs','r',encoding='utf-8').read()
old = '''        self.fire_trackers
            .insert(rule.name.clone(), FireTracker {
                last_fire: Instant::now(),
                fires_this_minute: 0,
                minute_start: Instant::now(),
            });'''
new = '''        self.fire_trackers
            .insert(rule.name.clone(), FireTracker {
                last_fire: Instant::now() - std::time::Duration::from_secs(3600),
                fires_this_minute: 0,
                minute_start: Instant::now(),
            });'''
c = c.replace(old, new)
open('crates/openfang-runtime/src/direct_channel.rs','w',encoding='utf-8').write(c)
print('Fixed FireTracker initial last_fire')
