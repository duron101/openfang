c=open('crates/openfang-kernel/Cargo.toml','r',encoding='utf-8').read()
c=c.replace('hex = { workspace = true }','hex = { workspace = true }\nhmac = { workspace = true }\nsha2 = { workspace = true }')
open('crates/openfang-kernel/Cargo.toml','w',encoding='utf-8').write(c)
print('added')
