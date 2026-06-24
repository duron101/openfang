import re

# Fix 1: monitor.clone()
c = open('crates/openfang-runtime/src/comm_monitor.rs','r',encoding='utf-8').read()
c = c.replace('let handle = monitor.start(rx);','let handle = monitor.clone().start(rx);')
open('crates/openfang-runtime/src/comm_monitor.rs','w',encoding='utf-8').write(c)
print('Fixed monitor clone')

# Fix 2: InsufficientSigners typo
c = open('crates/openfang-kernel/src/self_destruct.rs','r',encoding='utf-8').read()
c = c.replace('InsufficientSigners','InsufficientSignatures')
open('crates/openfang-kernel/src/self_destruct.rs','w',encoding='utf-8').write(c)
print('Fixed typo')
