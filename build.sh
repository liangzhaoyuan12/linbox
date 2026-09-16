#!/bin/bash
# build.sh — 编译 linbox 并打包 deb / rpm / pacman / tar.gz
# 用法：./build.sh [deb|rpm|pacman|tar.gz|all]
# 在任意架构上运行（x86_64 / loongarch64 / aarch64 …），打包当前架构。

set -e

# ── 项目信息 ──────────────────────────────────────────────────────────────
APP_NAME="linbox"
APP_ID="org.linbox.App"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
ARCH=$(uname -m)
BUILD_DIR="target/release"
BIN="$BUILD_DIR/$APP_NAME"
DEB_DIR="target/package/deb"
RPM_DIR="target/package/rpm"
PACMAN_DIR="target/package/pacman"
TAR_DIR="target/package/tar"

# ── 架构映射 ──────────────────────────────────────────────────────────────
case "$ARCH" in
    x86_64)          DEB_ARCH="amd64";   RPM_ARCH="x86_64";   PAC_ARCH="x86_64";   ;;
    aarch64)         DEB_ARCH="arm64";   RPM_ARCH="aarch64";  PAC_ARCH="aarch64";  ;;
    loongarch64)     DEB_ARCH="loong64"; RPM_ARCH="loongarch64"; PAC_ARCH="loongarch64"; ;;
    armv7l|armhf)    DEB_ARCH="armhf";   RPM_ARCH="armv7hl";  PAC_ARCH="armv7h";   ;;
    *)               DEB_ARCH="$ARCH";   RPM_ARCH="$ARCH";    PAC_ARCH="$ARCH";    ;;
esac

# ── 依赖列表 ──────────────────────────────────────────────────────────────
# 运行时必需的共享库和命令行工具
RUNTIME_DEPS_DEB="libgtk-4-1 (>= 4.14), libadwaita-1-0 (>= 1.4), libsqlite3-0 (>= 3.40), git (>= 2.30), p7zip-full, ffmpeg, pkexec"
RUNTIME_DEPS_RPM="gtk4 >= 4.14, libadwaita >= 1.4, sqlite-libs >= 3.40, git >= 2.30, p7zip, p7zip-plugins"
RUNTIME_DEPS_PAC="gtk4>=4.14 libadwaita>=1.4 sqlite ffmpeg git p7zip"

# ── 构建 ──────────────────────────────────────────────────────────────────
echo "▶ 编译 $APP_NAME v$VERSION ($ARCH) ..."
cargo build --release
if [ ! -f "$BIN" ]; then
    echo "✗ 编译失败：$BIN 不存在"
    exit 1
fi
echo "✓ 编译完成: $BIN ($(du -h "$BIN" | cut -f1))"

# ── .desktop 文件 ─────────────────────────────────────────────────────────
DESKTOP_CONTENT="[Desktop Entry]
Name=$APP_NAME
Comment=备忘录 + 工具箱
Exec=/usr/bin/$APP_NAME
Icon=accessories-text-editor-symbolic
Type=Application
Categories=Utility;
StartupWMClass=$APP_ID
"

# ── 打包函数 ──────────────────────────────────────────────────────────────

build_deb() {
    echo ""
    echo "▶ 打包 deb ($DEB_ARCH) ..."
    rm -rf "$DEB_DIR"
    local PKG="$DEB_DIR/${APP_NAME}_${VERSION}_${DEB_ARCH}"
    mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/applications" "$PKG/usr/share/icons/hicolor/scalable/apps"

    # 控制文件
    cat > "$PKG/DEBIAN/control" << EOF
Package: $APP_NAME
Version: $VERSION
Section: utils
Priority: optional
Architecture: $DEB_ARCH
Depends: $RUNTIME_DEPS_DEB
Installed-Size: $(du -sk "$BIN" | cut -f1)
Maintainer: liangzhaoyuan12 <liangzhaoyuan12@outlook.com>
Description: linbox - 备忘录 + 工具箱
 支持备忘录（图文混排、云同步）、媒体转换、系统监视等功能。
EOF

    cp "$BIN" "$PKG/usr/bin/$APP_NAME"
    chmod 755 "$PKG/usr/bin/$APP_NAME"
    echo "$DESKTOP_CONTENT" > "$PKG/usr/share/applications/$APP_NAME.desktop"

    dpkg-deb --build --root-owner-group "$PKG" "$DEB_DIR/${APP_NAME}_${VERSION}_${DEB_ARCH}.deb"
    echo "✓ deb: $DEB_DIR/${APP_NAME}_${VERSION}_${DEB_ARCH}.deb"
}

build_rpm() {
    echo ""
    echo "▶ 打包 rpm ($RPM_ARCH) ..."
    rm -rf "$RPM_DIR"
    local TOPDIR="$RPM_DIR/rpmbuild"
    mkdir -p "$TOPDIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

    # 把二进制和 desktop 文件放到 SOURCES（%install 用绝对路径复制到 buildroot）
    cp "$BIN" "$TOPDIR/SOURCES/$APP_NAME"
    local ABS_SRC="$(cd "$TOPDIR/SOURCES" && pwd)"

    local SPEC="$TOPDIR/SPECS/$APP_NAME.spec"
    cat > "$SPEC" << EOF
Name:           $APP_NAME
Version:        $VERSION
Release:        1
Summary:        备忘录 + 工具箱
License:        MIT
URL:            https://github.com/liangzhaoyuan12/linbox
BuildArch:      $RPM_ARCH
Requires:       $RUNTIME_DEPS_RPM

%description
linbox - 支持备忘录（图文混排、云同步）、媒体转换、系统监视等功能。

%install
mkdir -p %{buildroot}/usr/bin
mkdir -p %{buildroot}/usr/share/applications
cp $ABS_SRC/$APP_NAME %{buildroot}/usr/bin/$APP_NAME
chmod 755 %{buildroot}/usr/bin/$APP_NAME
cat > %{buildroot}/usr/share/applications/$APP_NAME.desktop << 'DESKTOP_EOF'
$DESKTOP_CONTENT
DESKTOP_EOF

%files
/usr/bin/$APP_NAME
/usr/share/applications/$APP_NAME.desktop
EOF

    local ABS_TOPDIR="$(cd "$TOPDIR" && pwd)"
    (rpmbuild -bb --define "_topdir $ABS_TOPDIR" --dbpath /tmp/rpm-db "$SPEC" 2>&1) | tail -5

    local RPM_FILE=$(find "$ABS_TOPDIR/RPMS" -name "*.rpm" 2>/dev/null | head -1)
    if [ -n "$RPM_FILE" ]; then
        cp "$RPM_FILE" "$RPM_DIR/"
        echo "✓ rpm: $RPM_DIR/$(basename "$RPM_FILE")"
    else
        echo "✗ rpm 打包失败"
    fi
}

build_pacman() {
    echo ""
    echo "▶ 打包 pacman ($PAC_ARCH) ..."
    rm -rf "$PACMAN_DIR"
    local PKG="$PACMAN_DIR/$APP_NAME"
    mkdir -p "$PKG" "$PKG/usr/bin" "$PKG/usr/share/applications"

    cp "$BIN" "$PKG/usr/bin/$APP_NAME"
    chmod 755 "$PKG/usr/bin/$APP_NAME"
    echo "$DESKTOP_CONTENT" > "$PKG/usr/share/applications/$APP_NAME.desktop"

    cat > "$PKG/PKGBUILD" << EOF
# Maintainer: liangzhaoyuan12 <liangzhaoyuan12@outlook.com>
pkgname=$APP_NAME
pkgver=$VERSION
pkgrel=1
pkgdesc="备忘录 + 工具箱"
arch=('$PAC_ARCH')
url="https://github.com/liangzhaoyuan12/linbox"
license=('MIT')
depends=($RUNTIME_DEPS_PAC)
source=()
noextract=()

package() {
    cp -a "\$startdir/usr" "\$pkgdir/"
}
EOF

    local OLD_DIR="$(pwd)"
    cd "$PKG"
    makepkg -f --nodeps --nocheck --skippgpcheck 2>&1 | tail -3
    cd "$OLD_DIR"
    local PKG_FILE=$(find "$PACMAN_DIR" -name "*.pkg.tar.*" -type f 2>/dev/null | head -1)
    if [ -n "$PKG_FILE" ]; then
        cp "$PKG_FILE" "$PACMAN_DIR/"
        echo "✓ pacman: $PACMAN_DIR/$(basename "$PKG_FILE")"
    else
        echo "✗ pacman 打包失败"
    fi
}

build_tar() {
    echo ""
    echo "▶ 打包 tar.gz ..."
    rm -rf "$TAR_DIR"
    local STAGING="$TAR_DIR/$APP_NAME-$VERSION-$ARCH"
    mkdir -p "$STAGING/usr/bin" "$STAGING/usr/share/applications"

    cp "$BIN" "$STAGING/usr/bin/$APP_NAME"
    chmod 755 "$STAGING/usr/bin/$APP_NAME"
    echo "$DESKTOP_CONTENT" > "$STAGING/usr/share/applications/$APP_NAME.desktop"

    tar -czf "$TAR_DIR/${APP_NAME}-${VERSION}-${ARCH}.tar.gz" -C "$TAR_DIR" "$APP_NAME-$VERSION-$ARCH"
    echo "✓ tar.gz: $TAR_DIR/${APP_NAME}-${VERSION}-${ARCH}.tar.gz"
}

# ── 执行 ──────────────────────────────────────────────────────────────────
TARGET="${1:-all}"

case "$TARGET" in
    deb)     build_deb ;;
    rpm)     build_rpm ;;
    pacman)  build_pacman ;;
    tar.gz)  build_tar ;;
    all)
        build_deb
        build_rpm
        build_pacman
        build_tar
        ;;
    *)
        echo "用法: $0 [deb|rpm|pacman|tar.gz|all]"
        exit 1
        ;;
esac

echo ""
echo "═══════════════════════════════════════"
echo "  打包完成  v$VERSION ($ARCH)"
echo "═══════════════════════════════════════"
ls -lh target/package/*/*.{deb,rpm,pkg.tar.*,tar.gz} 2>/dev/null
