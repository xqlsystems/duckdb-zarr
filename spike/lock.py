import duckdb, subprocess, sys, os, tempfile
mode = sys.argv[1]
EXT = os.path.abspath('build/debug/zarr.duckdb_extension')
db = os.path.join(tempfile.mkdtemp(), 'lock.duckdb')
con = duckdb.connect(db, config={'allow_unsigned_extensions':'true'})
if mode != 'none': con.execute(f"LOAD '{EXT}'")
if mode == 'call': con.execute("CALL zarr_attach('test/fixtures/xarray_tutorial/multi_dim_group.zarr', 'm')")
print('before close', flush=True)
con.close()
print('closed', flush=True)
p = subprocess.run([sys.executable, '-c', f"import duckdb; duckdb.connect({db!r}).execute('select 1'); print('other process opened file')"], capture_output=True, text=True, timeout=20)
print('other:', (p.stdout + p.stderr).strip().splitlines()[-1][:200], flush=True)
