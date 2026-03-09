# Maintainer: Stephen E. Hellings <stephen@hellings.cc>

pkgname=wo-git
pkgver=0.1.0
pkgrel=1
pkgdesc='The Electron Wayland Compositor'
arch=('x86_64')
url='https://github.com/hellings/wo'
license=('MIT')
depends=(
  'libdrm'
  'libinput'
  'libseat'
  'libxkbcommon'
  'mesa'
  'wayland'
  'xorg-xwayland'
  'pipewire'
  'electron'
)
makedepends=(
  'cargo'
  'rust'
  'clang'
  'pkgconf'
)
options=('!lto')

source=(
  "git+https://github.com/wo-wl/wo.git"
  "git+https://github.com/wo-wl/comraw.git"
  "config.example.toml"
  "packaging/wayland-sessions/wo.desktop"
  "electron"
)

build() {
  cd "$startdir"
  cargo build --release --locked --bins
}

package() {
  cd "$startdir"

  install -Dm755 target/release/wo "$pkgdir/usr/bin/wo"
  install -Dm755 target/release/wo-portal "$pkgdir/usr/bin/wo-portal"

  install -Dm644 packaging/wayland-sessions/wo.desktop \
    "$pkgdir/usr/share/wayland-sessions/wo.desktop"

  install -Dm644 config.example.toml \
    "$pkgdir/usr/share/doc/$pkgname/config.example.toml"

  if [[ -d electron ]]; then
    cp -a electron "$pkgdir/usr/lib/$pkgname/"
  fi
}
