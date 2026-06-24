import os
d = os.path.expanduser('~/.openfang')
os.makedirs(d, exist_ok=True)
c = open('openfang.toml.example','r').read()
open(f'{d}/config.toml','w').write(c)
print(f'Config written to {d}/config.toml')
