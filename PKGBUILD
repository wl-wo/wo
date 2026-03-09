# Maintainer: Stephen E. Hellings <stephen@hellings.cc>

pkgname=wo-git
pkgver=0.1.0.r2.ge442ad4
pkgrel=1
pkgdesc='The Electron Wayland Compositor'
arch=('x86_64')
url='https://github.com/wl-wo/wo'
license=('MIT')
provides=('wo')
conflicts=('wo')
depends=(
  'electron'
  'libdrm'
  'libinput'
  'seatd'
  'libxkbcommon'
  'mesa'
  'pipewire'
  'wayland'
  'xdg-desktop-portal'
  'xorg-xwayland'
)
makedepends=(
  'bun'
  'cargo'
  'clang'
  'npm'
  'pkgconf'
  'rust'
)
options=('!lto')

source=(
  "wo::git+https://github.com/wl-wo/wo.git"
  "comraw::git+https://github.com/wl-wo/comraw.git"
)
sha256sums=('SKIP' 'SKIP')

pkgver() {
  cd "$srcdir/wo"
  printf '0.1.0.r%s.g%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

build() {
  cd "$srcdir/wo"
  cargo build --release --locked --bins

  cd "$srcdir/wo/electron"
  bun install --frozen-lockfile
  bun run build
  bun run build-native

  cd "$srcdir/wo/electron/types"
  bun install

  cd "$srcdir/comraw"
  npm install --no-audit --no-fund
  npm run build
}

package() {
  cd "$srcdir/wo"

  install -Dm755 target/release/wo "$pkgdir/usr/bin/wo"
  install -Dm755 target/release/wo-portal "$pkgdir/usr/bin/wo-portal"

  install -Dm644 packaging/wayland-sessions/wo.desktop \
    "$pkgdir/usr/share/wayland-sessions/wo.desktop"

  install -Dm644 config.example.toml \
    "$pkgdir/usr/share/doc/$pkgname/config.example.toml"

  install -d "$pkgdir/usr/lib/wo"
  cp -a electron/dist "$pkgdir/usr/lib/wo/"
  cp -a electron/native "$pkgdir/usr/lib/wo/"

  install -d "$pkgdir/usr/lib/wo/comraw"
  cp -a "$srcdir/comraw/dist" "$pkgdir/usr/lib/wo/comraw/"
}
