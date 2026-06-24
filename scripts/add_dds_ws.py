c=open('Cargo.toml','r',encoding='utf-8').read()
c=c.replace('"crates/openfang-platform-arksim",','"crates/openfang-platform-arksim",\n    "crates/openfang-platform-dds",')
open('Cargo.toml','w',encoding='utf-8').write(c)
print('registered')
