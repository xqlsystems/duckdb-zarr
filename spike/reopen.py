import duckdb, sys, os, tempfile
EXT = os.path.abspath('build/debug/zarr.duckdb_extension')
db = os.path.join(tempfile.mkdtemp(), 'r.duckdb')
con = duckdb.connect(db, config={'allow_unsigned_extensions':'true'}); con.execute(f"LOAD '{EXT}'"); con.close()
print('reopen same config', flush=True)
c = duckdb.connect(db, config={'allow_unsigned_extensions':'true'}); print('ok same config:', c.execute('select 1').fetchall(), flush=True); c.close()
print('reopen new config', flush=True)
try:
    c = duckdb.connect(db, config={'threads': '2'}); print('ok new config', flush=True)
except Exception as e: print('ERR', str(e)[:200], flush=True)
