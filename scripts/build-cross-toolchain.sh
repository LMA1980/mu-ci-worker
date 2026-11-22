#!/usr/bin/env bash
# ==============================================================================
# build-cross-toolchain.sh
# Unified Native + All-to-All Cross-Compilation Toolchain Builder
# Supports x86_64, aarch64, and riscv64 (RVA23U64, SpacemiT X100 & A100)
# Designed for Debian/Ubuntu CI workers ('node-a', 'worker-riscv64', 'worker-aarch64')
# ==============================================================================

set -euo pipefail

PREFIX_BASE="${PREFIX_BASE:-/opt}"
BUILD_ROOT="${BUILD_ROOT:-/var/lib/containers/cross-toolchain-build}"
JOBS="${JOBS:-$(nproc)}"
NICE_CMD="${NICE_CMD:-nice -n 10}"

GCC_INSTALL_DIR="${PREFIX_BASE}/gcc-17"
LLVM_INSTALL_DIR="${PREFIX_BASE}/llvm-24"
SYSROOT_BASE="${SYSROOT_BASE:-${PREFIX_BASE}/sysroots}"
DESTDIR="${DESTDIR:-}"

HOST_ARCH="${HOST_ARCH:-$(uname -m)}"
normalize_host_arch() {
    case "$1" in
        x86_64|amd64)   HOST_ARCH="x86_64"; HOST_TRIPLE="x86_64-linux-gnu"; LLVM_DEFAULT_TRIPLE="x86_64-unknown-linux-gnu" ;;
        aarch64|arm64)  HOST_ARCH="aarch64"; HOST_TRIPLE="aarch64-linux-gnu"; LLVM_DEFAULT_TRIPLE="aarch64-unknown-linux-gnu" ;;
        riscv64)        HOST_ARCH="riscv64"; HOST_TRIPLE="riscv64-linux-gnu"; LLVM_DEFAULT_TRIPLE="riscv64-unknown-linux-gnu" ;;
        *) echo "Unsupported HOST_ARCH: $1" >&2; exit 1 ;;
    esac
}
normalize_host_arch "${HOST_ARCH}"

BUILD_ARCH="$(uname -m)"
case "${BUILD_ARCH}" in
    x86_64|amd64)   BUILD_TRIPLE="x86_64-linux-gnu" ;;
    aarch64|arm64)  BUILD_TRIPLE="aarch64-linux-gnu" ;;
    riscv64)        BUILD_TRIPLE="riscv64-linux-gnu" ;;
    *)              BUILD_TRIPLE="${HOST_TRIPLE}" ;;
esac

CANADIAN_CROSS=0
if [ "${BUILD_TRIPLE}" != "${HOST_TRIPLE}" ]; then
    CANADIAN_CROSS=1
fi

BINUTILS_GIT="${BINUTILS_GIT:-https://sourceware.org/git/binutils-gdb.git}"
BINUTILS_REF="${BINUTILS_REF:-master}"
GCC_GIT="${GCC_GIT:-https://github.com/gcc-mirror/gcc.git}"
GCC_REF="${GCC_REF:-master}"
LLVM_GIT="${LLVM_GIT:-https://github.com/llvm/llvm-project.git}"
LLVM_REF="${LLVM_REF:-main}"
LLVM_TABLEGEN="${LLVM_TABLEGEN:-${BUILD_ROOT}/host-tblgen/llvm-tblgen}"
CLANG_TABLEGEN="${CLANG_TABLEGEN:-${BUILD_ROOT}/host-tblgen/clang-tblgen}"

ACTION="all"
while [ $# -gt 0 ]; do
    case "$1" in
        --host-arch)
            normalize_host_arch "$2"
            CANADIAN_CROSS=0
            [ "${BUILD_TRIPLE}" != "${HOST_TRIPLE}" ] && CANADIAN_CROSS=1
            shift 2
            ;;
        --destdir)
            DESTDIR="$2"
            shift 2
            ;;
        prereqs|sysroots|binutils|gcc|llvm|wrappers|all)
            ACTION="$1"
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [--host-arch x86_64|aarch64|riscv64] [--destdir <dir>] [prereqs|sysroots|binutils|gcc|llvm|wrappers|all]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

TARGET_TRIPLES=("x86_64-linux-gnu" "aarch64-linux-gnu" "riscv64-linux-gnu")

echo "======================================================================"
echo " Unified Multi-Arch Toolchain Builder (Binutils + GCC 17 + LLVM 24)"
echo " Build Host:  ${BUILD_TRIPLE}"
echo " Target Host: ${HOST_TRIPLE} (Canadian cross: ${CANADIAN_CROSS})"
echo " Targets:     x86_64, aarch64 (cortex-a72), riscv64 (RVA23U64, X100, A100)"
echo " Build Jobs:  ${JOBS}"
echo " Install Dir: ${DESTDIR}${PREFIX_BASE}"
echo "======================================================================"

sysroot_for_target() {
    local target="$1"
    if [ "${target}" = "${HOST_TRIPLE}" ]; then
        echo "/"
    else
        echo "${SYSROOT_BASE}/${target}"
    fi
}

install_prereqs() {
    echo "==> Checking host build prerequisites..."
    if command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -qq
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
            build-essential cmake ninja-build git python3 pkg-config \
            libgmp-dev libmpfr-dev libmpc-dev flex bison texinfo \
            libzstd-dev zlib1g-dev lld dpkg-dev
    fi
}

build_sysroots() {
    echo "==> Building unified Debian cross-sysroots in ${DESTDIR}${SYSROOT_BASE}..."
    local work
    work="$(mktemp -d)"
    trap 'rm -rf "${work}"' RETURN
    mkdir -p "${work}/debs" "${work}/sysroots"
    (
        cd "${work}/debs"
        apt-get download \
            libc6-amd64-cross libc6-dev-amd64-cross linux-libc-dev-amd64-cross \
            libc6-arm64-cross libc6-dev-arm64-cross linux-libc-dev-arm64-cross \
            libc6-riscv64-cross libc6-dev-riscv64-cross linux-libc-dev-riscv64-cross >/dev/null
    )

    unpack_one() {
        local triple="$1" deb_arch="$2"
        local tmp="${work}/unpack-${triple}"
        local dest="${work}/sysroots/${triple}"
        mkdir -p "${tmp}" "${dest}/usr"
        for pkg in "libc6-${deb_arch}-cross" "libc6-dev-${deb_arch}-cross" "linux-libc-dev-${deb_arch}-cross"; do
            dpkg-deb -x "${work}/debs/${pkg}"_*.deb "${tmp}"
        done
        mv "${tmp}/usr/${triple}/include" "${dest}/include"
        mv "${tmp}/usr/${triple}/lib" "${dest}/lib"
        ln -sfn lib "${dest}/lib64"
        ln -sfn ../include "${dest}/usr/include"
        ln -sfn ../lib "${dest}/usr/lib"
        ln -sfn ../lib "${dest}/usr/lib64"
        ln -sfn .. "${dest}/usr/${triple}"
    }

    unpack_one "x86_64-linux-gnu" "amd64"
    unpack_one "aarch64-linux-gnu" "arm64"
    unpack_one "riscv64-linux-gnu" "riscv64"

    sudo mkdir -p "${DESTDIR}${SYSROOT_BASE}"
    sudo cp -a "${work}/sysroots/." "${DESTDIR}${SYSROOT_BASE}/"
    sudo chown -R root:root "${DESTDIR}${SYSROOT_BASE}"
}

sync_repo() {
    local url="$1" dir="$2" ref="$3"
    if [ ! -d "${dir}/.git" ]; then
        git clone --depth 1 "${url}" "${dir}"
    fi
    git -C "${dir}" fetch --depth 1 origin "${ref}"
    git -C "${dir}" checkout --detach FETCH_HEAD
}

build_binutils() {
    echo "==> Building upstream GNU Binutils for host=${HOST_TRIPLE}..."
    mkdir -p "${BUILD_ROOT}/src" "${BUILD_ROOT}/build"
    sync_repo "${BINUTILS_GIT}" "${BUILD_ROOT}/src/binutils-gdb" "${BINUTILS_REF}"

    for target in "${TARGET_TRIPLES[@]}"; do
        local bdir="${BUILD_ROOT}/build/binutils-${HOST_ARCH}-${target}"
        local sysroot
        sysroot="$(sysroot_for_target "${target}")"
        rm -rf "${bdir}" && mkdir -p "${bdir}"
        (
            cd "${bdir}"
            local extra=()
            if [ "${target}" = "${HOST_TRIPLE}" ]; then
                extra+=(--with-lib-path="/usr/local/lib/${target}:/lib/${target}:/usr/lib/${target}:/usr/local/lib:/lib:/usr/lib")
            fi
            local host_env=(CFLAGS="-O2" CXXFLAGS="-O2")
            if [ "${CANADIAN_CROSS}" -eq 1 ]; then
                host_env+=(
                    "CC=${HOST_TRIPLE}-gcc"
                    "CXX=${HOST_TRIPLE}-g++"
                    "AR=${HOST_TRIPLE}-ar"
                    "RANLIB=${HOST_TRIPLE}-ranlib"
                )
            fi
            env "${host_env[@]}" \
            ${NICE_CMD} "${BUILD_ROOT}/src/binutils-gdb/configure" \
                --prefix="${GCC_INSTALL_DIR}" \
                --build="${BUILD_TRIPLE}" \
                --host="${HOST_TRIPLE}" \
                --target="${target}" \
                --with-sysroot="${sysroot}" \
                "${extra[@]}" \
                --disable-gdb --disable-gdbserver --disable-sim --disable-gprofng \
                --disable-nls --disable-werror --enable-compressed-debug-sections=all
            ${NICE_CMD} make -j"${JOBS}"
            if [ "${CANADIAN_CROSS}" -eq 1 ]; then
                sudo make DESTDIR="${DESTDIR}" install
            else
                sudo make DESTDIR="${DESTDIR}" install-strip
            fi
        )
    done
}

build_gcc17() {
    echo "==> Building upstream GCC 17 for host=${HOST_TRIPLE}..."
    mkdir -p "${BUILD_ROOT}/src" "${BUILD_ROOT}/build"
    sync_repo "${GCC_GIT}" "${BUILD_ROOT}/src/gcc" "${GCC_REF}"
    (cd "${BUILD_ROOT}/src/gcc" && [ -e gmp ] || ./contrib/download_prerequisites)

    export PATH="${GCC_INSTALL_DIR}/bin:${PATH}"
    for target in "${TARGET_TRIPLES[@]}"; do
        local bdir="${BUILD_ROOT}/build/gcc17-${HOST_ARCH}-${target}"
        rm -rf "${bdir}" && mkdir -p "${bdir}"
        (
            cd "${bdir}"
            local arch_opts=()
            case "${target}" in
                aarch64-linux-gnu)  arch_opts+=(--with-cpu=cortex-a72) ;;
                riscv64-linux-gnu)  arch_opts+=(--with-arch=rva23u64 --with-tune=spacemit-x100 --with-abi=lp64d) ;;
            esac

            local sysroot_opts=()
            if [ "${target}" = "${HOST_TRIPLE}" ] && [ "${CANADIAN_CROSS}" -eq 0 ]; then
                sysroot_opts+=(--enable-multiarch)
            elif [ "${target}" = "${HOST_TRIPLE}" ] && [ "${CANADIAN_CROSS}" -eq 1 ]; then
                sysroot_opts+=(--enable-multiarch --with-sysroot=/ --with-build-sysroot="${SYSROOT_BASE}/${target}")
            else
                sysroot_opts+=(--with-sysroot="$(sysroot_for_target "${target}")")
                if [ "${CANADIAN_CROSS}" -eq 1 ]; then
                    sysroot_opts+=(--with-build-sysroot="${SYSROOT_BASE}/${target}")
                fi
            fi

            local host_env=(CFLAGS="-O2" CXXFLAGS="-O2" CFLAGS_FOR_TARGET="-O2" CXXFLAGS_FOR_TARGET="-O2")
            if [ "${CANADIAN_CROSS}" -eq 1 ]; then
                host_env+=(
                    "CC=/usr/bin/${HOST_TRIPLE}-gcc"
                    "CXX=/usr/bin/${HOST_TRIPLE}-g++"
                    "AR=/usr/bin/${HOST_TRIPLE}-ar"
                    "AS=/usr/bin/${HOST_TRIPLE}-as"
                    "LD=/usr/bin/${HOST_TRIPLE}-ld"
                    "NM=/usr/bin/${HOST_TRIPLE}-nm"
                    "RANLIB=/usr/bin/${HOST_TRIPLE}-ranlib"
                    "STRIP=/usr/bin/${HOST_TRIPLE}-strip"
                    "CC_FOR_BUILD=/usr/bin/gcc"
                    "CXX_FOR_BUILD=/usr/bin/g++"
                    "CC_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-gcc"
                    "CXX_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-g++"
                    "GCC_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-gcc"
                    "AR_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-ar"
                    "AS_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-as"
                    "LD_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-ld"
                    "NM_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-nm"
                    "RANLIB_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-ranlib"
                    "STRIP_FOR_TARGET=${GCC_INSTALL_DIR}/bin/${target}-strip"
                )
            fi

            env "${host_env[@]}" \
            ${NICE_CMD} "${BUILD_ROOT}/src/gcc/configure" \
                --prefix="${GCC_INSTALL_DIR}" \
                --build="${BUILD_TRIPLE}" \
                --host="${HOST_TRIPLE}" \
                --target="${target}" \
                "${sysroot_opts[@]}" \
                "${arch_opts[@]}" \
                --enable-languages=c,c++ \
                --disable-multilib \
                --disable-bootstrap \
                --disable-nls \
                --enable-shared \
                --enable-threads=posix \
                --enable-checking=release
            ${NICE_CMD} make -j"${JOBS}"
            if [ "${CANADIAN_CROSS}" -eq 1 ]; then
                sudo make DESTDIR="${DESTDIR}" install
            else
                sudo make DESTDIR="${DESTDIR}" install-strip
            fi
        )
    done
}

build_llvm24() {
    echo "==> Building upstream LLVM 24 (clang + lld) for host=${HOST_TRIPLE}..."
    mkdir -p "${BUILD_ROOT}/src" "${BUILD_ROOT}/build" "${BUILD_ROOT}/host-tblgen"
    sync_repo "${LLVM_GIT}" "${BUILD_ROOT}/src/llvm-project" "${LLVM_REF}"

    local bdir="${BUILD_ROOT}/build/llvm24-${HOST_ARCH}"
    rm -rf "${bdir}" && mkdir -p "${bdir}"
    local cmake_args=(
        -G Ninja
        -DCMAKE_BUILD_TYPE=Release
        -DCMAKE_INSTALL_PREFIX="${LLVM_INSTALL_DIR}"
        -DLLVM_ENABLE_PROJECTS="clang;lld"
        -DLLVM_TARGETS_TO_BUILD="X86;AArch64;RISCV;WebAssembly"
        -DLLVM_DEFAULT_TARGET_TRIPLE="${LLVM_DEFAULT_TRIPLE}"
        -DLLVM_OPTIMIZED_TABLEGEN=ON
        -DLLVM_INCLUDE_BENCHMARKS=OFF
        -DLLVM_INCLUDE_EXAMPLES=OFF
        -DLLVM_INCLUDE_TESTS=OFF
        -DLLVM_ENABLE_ASSERTIONS=OFF
        -DLLVM_PARALLEL_LINK_JOBS=2
    )
    if [ "${CANADIAN_CROSS}" -eq 1 ]; then
        local cross_sysroot="${BUILD_ROOT}/${HOST_ARCH}-llvm-sysroot"
        cmake_args+=(
            -DCMAKE_CROSSCOMPILING=ON
            -DCMAKE_SYSTEM_NAME=Linux
            -DCMAKE_SYSTEM_PROCESSOR="${HOST_ARCH}"
            -DCMAKE_C_COMPILER="/usr/bin/${HOST_TRIPLE}-gcc"
            -DCMAKE_CXX_COMPILER="/usr/bin/${HOST_TRIPLE}-g++"
            -DCMAKE_AR="/usr/bin/${HOST_TRIPLE}-ar"
            -DCMAKE_RANLIB="/usr/bin/${HOST_TRIPLE}-ranlib"
            -DCMAKE_STRIP="/usr/bin/${HOST_TRIPLE}-strip"
            -DCMAKE_C_FLAGS="-O2 -I${cross_sysroot}/usr/include"
            -DCMAKE_CXX_FLAGS="-O2 -I${cross_sysroot}/usr/include"
            -DCMAKE_EXE_LINKER_FLAGS="-L${cross_sysroot}/lib -Wl,-rpath-link,${cross_sysroot}/lib -fuse-ld=lld"
            -DCMAKE_SHARED_LINKER_FLAGS="-L${cross_sysroot}/lib -Wl,-rpath-link,${cross_sysroot}/lib -fuse-ld=lld"
            -DLLVM_HOST_TRIPLE="${LLVM_DEFAULT_TRIPLE}"
            -DLLVM_TABLEGEN="${LLVM_TABLEGEN}"
            -DCLANG_TABLEGEN="${CLANG_TABLEGEN}"
            -DLLVM_ENABLE_TERMINFO=OFF
            -DLLVM_ENABLE_LIBXML2=OFF
            -DLLVM_ENABLE_LIBEDIT=OFF
        )
    else
        cmake_args+=(
            -DCMAKE_C_FLAGS="-O2"
            -DCMAKE_CXX_FLAGS="-O2"
            -DLLVM_USE_LINKER=lld
        )
    fi

    cmake "${cmake_args[@]}" -B "${bdir}" "${BUILD_ROOT}/src/llvm-project/llvm"
    ${NICE_CMD} ninja -C "${bdir}" -j"${JOBS}"
    if [ "${CANADIAN_CROSS}" -eq 0 ] && [ -x "${bdir}/bin/llvm-tblgen" ]; then
        cp -f "${bdir}/bin/llvm-tblgen" "${BUILD_ROOT}/host-tblgen/llvm-tblgen"
        cp -f "${bdir}/bin/clang-tblgen" "${BUILD_ROOT}/host-tblgen/clang-tblgen"
    fi
    sudo DESTDIR="${DESTDIR}" cmake --build "${bdir}" --target install/strip
}

generate_wrappers() {
    echo "==> Installing target wrappers, Clang .cfg files, env profiles, and compatibility symlinks..."
    local gcc_bin="${DESTDIR}${GCC_INSTALL_DIR}/bin"
    local llvm_bin="${DESTDIR}${LLVM_INSTALL_DIR}/bin"
    local env_dir="${DESTDIR}${PREFIX_BASE}/toolchain-env"
    sudo mkdir -p "${gcc_bin}" "${llvm_bin}" "${env_dir}" \
        "${DESTDIR}${PREFIX_BASE}/spacemit-x100-env" "${DESTDIR}${PREFIX_BASE}/spacemit-a100-env"

    # Canonical native + triple symlinks
    if [ -x "${gcc_bin}/${HOST_TRIPLE}-gcc" ] && [ ! -e "${gcc_bin}/gcc" ]; then
        for t in gcc g++ cpp c++ ar as ld ld.bfd nm objcopy objdump ranlib readelf strip; do
            [ -e "${gcc_bin}/${HOST_TRIPLE}-${t}" ] && sudo ln -sfn "${HOST_TRIPLE}-${t}" "${gcc_bin}/${t}"
        done
    fi
    for t in gcc g++ cpp c++ ar as ld ld.bfd nm objcopy objdump ranlib readelf strip; do
        if [ -e "${gcc_bin}/${t}" ] && [ ! -e "${gcc_bin}/${HOST_TRIPLE}-${t}" ]; then
            sudo ln -sfn "${t}" "${gcc_bin}/${HOST_TRIPLE}-${t}"
        fi
    done

    for arch_pair in \
        "x86_64:x86_64-linux-gnu:x86_64-unknown-linux-gnu" \
        "aarch64:aarch64-linux-gnu:aarch64-unknown-linux-gnu" \
        "riscv64:riscv64-linux-gnu:riscv64-unknown-linux-gnu"; do
        IFS=":" read -r _ gnu_t unk_t <<< "${arch_pair}"
        for t in gcc g++ cpp c++ ar as ld ld.bfd nm objcopy objdump ranlib readelf strip; do
            if [ -e "${gcc_bin}/${gnu_t}-${t}" ] && [ ! -e "${gcc_bin}/${unk_t}-${t}" ]; then
                sudo ln -sfn "${gnu_t}-${t}" "${gcc_bin}/${unk_t}-${t}"
            fi
        done
        for c in clang clang++; do
            sudo tee "${llvm_bin}/${gnu_t}-${c}" >/dev/null << EOF
#!/usr/bin/env bash
exec "${LLVM_INSTALL_DIR}/bin/${c}" --target=${gnu_t} "\$@"
EOF
            sudo chmod 755 "${llvm_bin}/${gnu_t}-${c}"
            sudo ln -sfn "${gnu_t}-${c}" "${llvm_bin}/${unk_t}-${c}"
        done

        # Clang target configuration files
        local cfg_flags="--gcc-toolchain=${GCC_INSTALL_DIR} -fuse-ld=lld"
        if [ "${gnu_t}" != "${HOST_TRIPLE}" ]; then
            cfg_flags="--sysroot=${SYSROOT_BASE}/${gnu_t} ${cfg_flags}"
        fi
        printf '%s\n' "${cfg_flags}" | sudo tee "${llvm_bin}/${gnu_t}.cfg" >/dev/null
        printf '%s\n' "${cfg_flags}" | sudo tee "${llvm_bin}/${unk_t}.cfg" >/dev/null
    done

    # RVA23 + SpacemiT X100 / A100 driver wrappers
    for variant in \
        "rva23:-march=rva23u64 -mabi=lp64d" \
        "spacemit-x100:-mcpu=spacemit-x100 -mabi=lp64d" \
        "spacemit-a100:-mcpu=spacemit-a100 -mabi=lp64d"; do
        local vname="${variant%%:*}" vflags="${variant#*:}"
        for pair in \
            "gcc:riscv64-linux-gnu-gcc:${GCC_INSTALL_DIR}" \
            "g++:riscv64-linux-gnu-g++:${GCC_INSTALL_DIR}" \
            "clang:riscv64-linux-gnu-clang:${LLVM_INSTALL_DIR}" \
            "clang++:riscv64-linux-gnu-clang++:${LLVM_INSTALL_DIR}"; do
            IFS=":" read -r short_tool driver_name root_dir <<< "${pair}"
            sudo tee "${DESTDIR}${root_dir}/bin/riscv64-${vname}-linux-gnu-${short_tool}" >/dev/null << EOF
#!/usr/bin/env bash
exec "${root_dir}/bin/${driver_name}" ${vflags} "\$@"
EOF
            sudo chmod 755 "${DESTDIR}${root_dir}/bin/riscv64-${vname}-linux-gnu-${short_tool}"
        done
    done

    # Backward-compatible symlinks
    for link_name in gcc-17-spacemit-x100 gcc-17-spacemit-a100 cross-gcc-17; do
        sudo ln -sfn "${GCC_INSTALL_DIR}" "${DESTDIR}${PREFIX_BASE}/${link_name}"
    done
    for link_name in llvm-24-spacemit-x100 llvm-24-spacemit-a100 cross-llvm-24; do
        sudo ln -sfn "${LLVM_INSTALL_DIR}" "${DESTDIR}${PREFIX_BASE}/${link_name}"
    done

    # Unified /opt/toolchain-env/*.sh profiles
    local rv_sysroot="/"
    local x86_sysroot="/"
    local arm_sysroot="/"
    [ "${HOST_ARCH}" != "riscv64" ] && rv_sysroot="${SYSROOT_BASE}/riscv64-linux-gnu"
    [ "${HOST_ARCH}" != "x86_64" ] && x86_sysroot="${SYSROOT_BASE}/x86_64-linux-gnu"
    [ "${HOST_ARCH}" != "aarch64" ] && arm_sysroot="${SYSROOT_BASE}/aarch64-linux-gnu"

    sudo tee "${env_dir}/env.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export TOOLCHAIN_SYSROOTS="${SYSROOT_BASE}"
EOF
    sudo tee "${env_dir}/x86_64.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export CC_x86_64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/x86_64-linux-gnu-clang"
export CXX_x86_64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/x86_64-linux-gnu-clang++"
export AR_x86_64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/llvm-ar"
export CFLAGS_x86_64_unknown_linux_gnu="--target=x86_64-linux-gnu --sysroot=${x86_sysroot} -march=x86-64"
export CXXFLAGS_x86_64_unknown_linux_gnu="\${CFLAGS_x86_64_unknown_linux_gnu}"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${LLVM_INSTALL_DIR}/bin/x86_64-linux-gnu-clang"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=lld"
EOF
    sudo tee "${env_dir}/aarch64.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export CC_aarch64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/aarch64-linux-gnu-clang"
export CXX_aarch64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/aarch64-linux-gnu-clang++"
export AR_aarch64_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/llvm-ar"
export CFLAGS_aarch64_unknown_linux_gnu="--target=aarch64-linux-gnu --sysroot=${arm_sysroot} -mcpu=cortex-a72"
export CXXFLAGS_aarch64_unknown_linux_gnu="\${CFLAGS_aarch64_unknown_linux_gnu}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${LLVM_INSTALL_DIR}/bin/aarch64-linux-gnu-clang"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=cortex-a72 -C link-arg=-fuse-ld=lld"
EOF
    sudo tee "${env_dir}/riscv64-rva23.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export CC_riscv64a23_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-rva23-linux-gnu-clang"
export CXX_riscv64a23_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-rva23-linux-gnu-clang++"
export AR_riscv64a23_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/llvm-ar"
export CFLAGS_riscv64a23_unknown_linux_gnu="--target=riscv64-linux-gnu --sysroot=${rv_sysroot} -march=rva23u64 -mabi=lp64d"
export CXXFLAGS_riscv64a23_unknown_linux_gnu="\${CFLAGS_riscv64a23_unknown_linux_gnu}"
export CARGO_TARGET_RISCV64A23_UNKNOWN_LINUX_GNU_LINKER="${LLVM_INSTALL_DIR}/bin/riscv64-rva23-linux-gnu-clang"
export CARGO_TARGET_RISCV64A23_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+rva23u64 -C link-arg=-fuse-ld=lld"
EOF
    sudo tee "${env_dir}/riscv64-spacemit-x100.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export CC_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-x100-linux-gnu-clang"
export CXX_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-x100-linux-gnu-clang++"
export AR_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/llvm-ar"
export CFLAGS_riscv64gc_unknown_linux_gnu="--target=riscv64-linux-gnu --sysroot=${rv_sysroot} -mcpu=spacemit-x100 -mabi=lp64d"
export CXXFLAGS_riscv64gc_unknown_linux_gnu="\${CFLAGS_riscv64gc_unknown_linux_gnu}"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-x100-linux-gnu-clang"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+v -C target-cpu=spacemit-x100 -C link-arg=-fuse-ld=lld"
EOF
    sudo tee "${env_dir}/riscv64-spacemit-a100.sh" >/dev/null << EOF
export PATH="${LLVM_INSTALL_DIR}/bin:${GCC_INSTALL_DIR}/bin:\${PATH}"
export CC_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-a100-linux-gnu-clang"
export CXX_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-a100-linux-gnu-clang++"
export AR_riscv64gc_unknown_linux_gnu="${LLVM_INSTALL_DIR}/bin/llvm-ar"
export CFLAGS_riscv64gc_unknown_linux_gnu="--target=riscv64-linux-gnu --sysroot=${rv_sysroot} -mcpu=spacemit-a100 -mabi=lp64d"
export CXXFLAGS_riscv64gc_unknown_linux_gnu="\${CFLAGS_riscv64gc_unknown_linux_gnu}"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER="${LLVM_INSTALL_DIR}/bin/riscv64-spacemit-a100-linux-gnu-clang"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+v -C target-cpu=spacemit-a100 -C link-arg=-fuse-ld=lld"
EOF
    sudo chmod 755 "${env_dir}"/*.sh

    # Backward-compatible activation profiles for SpacemiT X100 and A100
    sudo cp -f "${env_dir}/riscv64-spacemit-x100.sh" "${DESTDIR}${PREFIX_BASE}/spacemit-x100-env/env.sh"
    sudo cp -f "${env_dir}/riscv64-spacemit-a100.sh" "${DESTDIR}${PREFIX_BASE}/spacemit-a100-env/env.sh"
}

case "${ACTION}" in
    prereqs)  install_prereqs ;;
    sysroots) build_sysroots ;;
    binutils) build_binutils ;;
    gcc)      build_gcc17 ;;
    llvm)     build_llvm24 ;;
    wrappers) generate_wrappers ;;
    all)
        install_prereqs
        build_sysroots
        build_binutils
        build_gcc17
        build_llvm24
        generate_wrappers
        echo "==> Unified multi-arch toolchain suite installed successfully!"
        ;;
esac
