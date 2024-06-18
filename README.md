# sk8brd - Simple remote board control software

## Server
`sk8brd-server` is still not there.. The only other implementation is [cdba-server](https://github.com/linux-msm/cdba/) for now.

## Usage
### Interactive client:
`cargo run -- -f <host> -p <port> -i <path/to/boot.img> -b <board> [-u user] [--power-cycle]`

Keybinds:
* `CTRL-A` +
  * `a` -> send a CTRL-A
  * `B` -> send a console break
  * `p` -> turn off board power
  * `P` -> turn on board power
  * `q` -> quit
  * `s` -> request a JSON status update (WIP)
  * `v` -> turn off USB VBUS
  * `V` -> turn on USB VBUS

### Non-interactive CLI:
`cargo run --bin sk8brd-cli -f <host> -i <path/to/boot.img> [-b board]`

Make sure `ssh-agent` is running and has your keys imported.

## License
`BSD-3-Clause`

## Credits
```
Author: Konrad Dybcio <konradybcio@kernel.org>
cdba contributors (for the original cdba implementation)
```