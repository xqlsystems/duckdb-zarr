import duckdb, subprocess, sys, os, tempfile, textwrap
EXT = os.path.abspath('build/debug/zarr.duckdb_extension')
STORE = 'test/fixtures/xarray_tutorial/multi_dim_group.zarr'
PY = sys.executable
def child(code, timeout=60):
    pre = f"import duckdb\nEXT={EXT!r}\nSTORE={STORE!r}\n"
    try:
        p = subprocess.run([PY, '-c', pre + textwrap.dedent(code)], capture_output=True, text=True, timeout=timeout)
        return (p.returncode, (p.stdout + p.stderr).strip()[-1500:])
    except subprocess.TimeoutExpired:
        return ('TIMEOUT', '')
def show(name, r): print(f'== {name}: rc={r[0]}\n{r[1]}\n')

show('1 basic CALL + query + SHOW ALL TABLES', child('''
con = duckdb.connect(config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'")
print(con.execute(f"CALL zarr_attach('{STORE}', 'm')").fetchall())
print(con.execute("SELECT database_name, schema_name, table_name FROM duckdb_views() WHERE database_name='m'").fetchall())
for (v,) in con.execute(f"CALL zarr_attach_list()").fetchall() if False else []: pass
print(con.execute('SELECT * FROM m."demo/nested".scalar').fetchall())
print(con.execute("SELECT table_name FROM information_schema.tables WHERE table_catalog='m'").fetchall())
'''))

show('2 query each mounted view', child('''
con = duckdb.connect(config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'")
views = [v for (v,) in con.execute(f"CALL zarr_attach('{STORE}', 'm')").fetchall()]
for v in views:
    parts = v.split('.', 2); q = f'SELECT COUNT(*) FROM m.{parts[1]}."{parts[2]}"' if not parts[1].startswith('"') else f'SELECT COUNT(*) FROM {v}'
    print(v, con.execute(q).fetchall())
'''))

show('3 inside explicit transaction', child('''
con = duckdb.connect(config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'")
con.execute("BEGIN")
print(con.execute(f"CALL zarr_attach('{STORE}', 'm')").fetchall())
print(con.execute('SELECT * FROM m."demo/nested".scalar').fetchall())
con.execute("COMMIT"); print('committed')
'''))

show('4 same alias twice', child('''
con = duckdb.connect(config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'")
con.execute(f"CALL zarr_attach('{STORE}', 'm')")
try: con.execute(f"CALL zarr_attach('{STORE}', 'm')")
except Exception as e: print('ERR', e)
'''))

show('5 visible from another connection (cursor)', child('''
con = duckdb.connect(config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'")
con.execute(f"CALL zarr_attach('{STORE}', 'm')")
cur = con.cursor(); print(cur.execute('SELECT * FROM m."demo/nested".scalar').fetchall())
'''))

db = os.path.join(tempfile.mkdtemp(), 'lock.duckdb')
for label, call in [('6a LOAD only', False), ('6b LOAD + CALL', True)]:
    if os.path.exists(db): os.remove(db)
    code = f'''
import subprocess, sys
con = duckdb.connect({db!r}, config={{'allow_unsigned_extensions':'true'}}); con.execute(f"LOAD '{{EXT}}'")
if {call}: con.execute(f"CALL zarr_attach('{{STORE}}', 'm')")
con.close()
# Another process opens the same file for writing while this one is still alive.
p = subprocess.run([sys.executable, '-c', "import duckdb; duckdb.connect({db!r}).execute('select 1'); print('other process opened file')"], capture_output=True, text=True)
print((p.stdout + p.stderr).strip()[-400:])
# Same process reopens the file with a different config (forces a new instance).
try:
    c2 = duckdb.connect({db!r}, config={{'threads': '2'}}); print('same process reopened with new config')
except Exception as e: print('same-process reopen ERR', str(e)[:200])
'''
    show(f'{label}: file lock released after close?', child(code))
