
## Install

Download `turborm-…-x86_64-linux.tar.gz` below, then:

```sh
tar xzf turborm-*-x86_64-linux.tar.gz
cd turborm-*-x86_64-linux
./install.sh
```

This installs TurboRM for your user only (`~/.local/bin`, plus an entry in your application menu). `./install.sh --uninstall` removes it again.

PDF pages need Poppler: `sudo apt install poppler-utils` on Debian/Ubuntu. Pairing needs a desktop keyring (GNOME Keyring, or KWallet with Secret Service). Built on Ubuntu 22.04 for x86_64; it runs on Debian 12, Ubuntu 22.04 and newer distributions.

Check the download with `sha256sum -c turborm-*-x86_64-linux.tar.gz.sha256`.

TurboRM is unofficial and not affiliated with reMarkable AS. Keep backups of your documents.
