# Python cdba example

This is a small Python application that uses the `sk8brd_cdba` module to list
available devices, boot a Linux boot image on a selected board, wait for a
console prompt, run a command, collect its output, and disconnect.

```sh
python -m pip install maturin
maturin develop
python examples/python_cdba/boot_linux.py \
  --host <host> \
  --board <board> \
  --image <path/to/boot.img> \
  --prompt 'root@qcom-armv8a:~#' \
  --command 'uname -a; id'
```

Useful options:

- `--port`, defaults to `22`
- `--user`, defaults to `cdba`
- `--timeout`, defaults to `120`
- `--prompt`, defaults to `root@qcom-armv8a:~#`
- `--command`, defaults to `uname -a; id`

The example intentionally drives the console through `read_console()` and
`write_console()` so it can be used as a starting point for more involved
Python tests.
