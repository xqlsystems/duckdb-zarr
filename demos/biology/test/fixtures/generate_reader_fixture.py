# /// script
# requires-python = ">=3.11"
# dependencies = ["zarr==3.2.1", "numpy==2.4.4"]
# ///
"""Generate the small synthetic format fixture; never used as demo data.

Run `uv run demos/biology/test/fixtures/generate_reader_fixture.py` to refresh
reader-regressions.json with original Zarr writer bytes and readback values.
"""

import asyncio
import base64
import json
from pathlib import Path

import numpy as np
import zarr
from zarr.storage import MemoryStore


async def main():
    """Write omitted-fill chunks and a named nested array with zarr-python."""
    store = MemoryStore()
    root = zarr.group(store=store, zarr_format=2)
    root.attrs['multiscales'] = [{
        'version': '0.4',
        'axes': [{'name': 'c', 'type': 'channel'}, *[
            {'name': axis, 'type': 'space', 'unit': 'micrometer'} for axis in ['z', 'y', 'x']
        ]],
        'datasets': [{'path': '0', 'coordinateTransformations': [{'type': 'scale', 'scale': [1, 1, 1, 1]}]}],
    }]
    root.attrs['omero'] = {'channels': [{'label': 'Synthetic format test signal', 'color': '0000FF'}]}
    array = root.create_array('0', shape=(1, 2, 2, 2), chunks=(1, 1, 2, 2), dtype='uint16',
                              fill_value=7, compressors=None, chunk_key_encoding={'name': 'v2', 'separator': '/'},
                              config={'write_empty_chunks': False})
    values = np.array([[[[7, 7], [7, 7]], [[8, 9], [10, 11]]]], dtype=np.uint16)
    array[:] = values
    array.attrs['_ARRAY_DIMENSIONS'] = ['c', 'z', 'y', 'x']
    labels = root.create_group('labels').create_array('nuclei', shape=(2, 2, 2), chunks=(1, 2, 2),
                                                     dtype='uint16', fill_value=0, compressors=None,
                                                     chunk_key_encoding={'name': 'v2', 'separator': '/'})
    labels[:] = np.array([[[0, 1], [1, 0]], [[2, 2], [3, 3]]], dtype=np.uint16)
    labels.attrs['_ARRAY_DIMENSIONS'] = ['z', 'y', 'x']
    files = {}
    async for key in store.list():
        value = await store.get(key)
        assert value is not None
        files[key] = base64.b64encode(value.to_bytes()).decode('ascii')
    assert '0/0/0/0/0' not in files
    np.testing.assert_array_equal(array[:], values)
    output = {
        'generator': {'zarr': zarr.__version__, 'numpy': np.__version__, 'synthetic_format_fixture': True},
        'metadata': {'level': '0', 'shape': list(array.shape), 'chunks': list(array.chunks), 'dimension_separator': '/'},
        'expected': {'missing_chunk': '0/0/0/0/0', 'fill_value': 7, 'intensities': np.asarray(array[:]).ravel().tolist(), 'labels': np.asarray(labels[:]).ravel().tolist()},
        'files': dict(sorted(files.items())),
    }
    destination = Path(__file__).with_name('reader-regressions.json')
    destination.write_text(json.dumps(output, indent=2) + '\n')
    print(f'Generated {len(files)} original Zarr objects: {destination}')


if __name__ == '__main__':
    asyncio.run(main())
