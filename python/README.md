# sk8brd-cdba Python module

This crate exposes the sk8brd cdba client as a Python module named
`sk8brd_cdba`.

Build it in a virtual environment with:

```sh
python -m pip install maturin
maturin develop
```

Then run the example application:

```sh
SK8BRD_FARM=<host> \
SK8BRD_BOARD=<board> \
SK8BRD_BOOT_IMAGE=<path/to/boot.img> \
python examples/python_cdba/boot_android.py
```
