#!/usr/bin/env bash
set -euo pipefail

readonly CARGO_AUDIT_VERSION="0.22.2"
readonly OSV_SCANNER_VERSION="2.3.8"

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
readonly repo_root
readonly target_dir="${repo_root}/target"
readonly security_tools_dir="${target_dir}/security-tools"
readonly rustsec_db="${security_tools_dir}/rustsec-advisory-db"
readonly cargo_home="${security_tools_dir}/cargo-home"
readonly rustsec_lock="${rustsec_db}..lock"
readonly cargo_audit_parent="${security_tools_dir}/cargo-audit"
readonly cargo_audit_dir="${security_tools_dir}/cargo-audit/v${CARGO_AUDIT_VERSION}"
readonly cargo_audit_bin="${cargo_audit_dir}/cargo-audit"
readonly osv_parent="${security_tools_dir}/osv-scanner"
readonly osv_dir="${security_tools_dir}/osv-scanner/v${OSV_SCANNER_VERSION}"
readonly osv_scanner="${osv_dir}/osv-scanner"
readonly osv_config="${repo_root}/osv-scanner.toml"
readonly rustsec_ignores=(
    "RUSTSEC-2023-0071"
    "RUSTSEC-2026-0098"
    "RUSTSEC-2026-0099"
    "RUSTSEC-2026-0104"
)

ensure_physical_directory() {
    local directory="$1"
    if [[ -L "${directory}" ]] || [[ -e "${directory}" && ! -d "${directory}" ]]; then
        echo "Refusing unsafe security-tool cache directory ${directory}." >&2
        exit 1
    fi
    if [[ ! -d "${directory}" ]]; then
        mkdir -- "${directory}"
    fi
    if [[ "$(cd -- "${directory}" && pwd -P)" != "${directory}" ]]; then
        echo "Security-tool cache directory resolved outside the repository: ${directory}." >&2
        exit 1
    fi
}

# Validate every parent before creating its child. A single recursive mkdir
# would follow an existing intermediate symlink before it could be rejected.
for directory in \
    "${target_dir}" \
    "${security_tools_dir}" \
    "${cargo_home}" \
    "${cargo_audit_parent}" \
    "${cargo_audit_dir}" \
    "${osv_parent}" \
    "${osv_dir}"; do
    ensure_physical_directory "${directory}"
done
if [[ -L "${rustsec_db}" ]] || [[ -e "${rustsec_db}" && ! -d "${rustsec_db}" ]]; then
    echo "Refusing unsafe RustSec advisory database path ${rustsec_db}." >&2
    exit 1
fi
if [[ -d "${rustsec_db}" ]] && [[ "$(cd -- "${rustsec_db}" && pwd -P)" != "${rustsec_db}" ]]; then
    echo "RustSec advisory database resolved outside the repository cache." >&2
    exit 1
fi
if [[ -L "${rustsec_lock}" ]] || [[ -e "${rustsec_lock}" && ! -f "${rustsec_lock}" ]]; then
    echo "Refusing unsafe RustSec advisory database lock path ${rustsec_lock}." >&2
    exit 1
fi

case "$(uname -s):$(uname -m)" in
    Linux:x86_64)
        cargo_asset="cargo-audit-x86_64-unknown-linux-musl-v${CARGO_AUDIT_VERSION}.tgz"
        cargo_checksum="7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428"
        osv_asset="osv-scanner_linux_amd64"
        osv_checksum="bc98e15319ed0d515e3f9235287ba53cdc5535d576d24fd573978ecfe9ab92dc"
        ;;
    Linux:aarch64 | Linux:arm64)
        cargo_asset="cargo-audit-aarch64-unknown-linux-gnu-v${CARGO_AUDIT_VERSION}.tgz"
        cargo_checksum="c6603814ddaa45e51263dafd31c0ac98808f688d26f7395804f9670b0fd599dd"
        osv_asset="osv-scanner_linux_arm64"
        osv_checksum="8158b18edd2d03b1a30d905ca91b032bc62262167be8f206c27114f08823e27c"
        ;;
    Darwin:x86_64)
        cargo_asset="cargo-audit-x86_64-apple-darwin-v${CARGO_AUDIT_VERSION}.tgz"
        cargo_checksum="847831323de932155b226ab60ee4a180e13e5d007a019f0d4b7b4d89a6de2ab2"
        osv_asset="osv-scanner_darwin_amd64"
        osv_checksum="b8a80a9f14ca4c0cd0fc2d351b28f740da9e6a5b18385ac9f9d083360b5b504e"
        ;;
    Darwin:arm64)
        cargo_asset="cargo-audit-aarch64-apple-darwin-v${CARGO_AUDIT_VERSION}.tgz"
        cargo_checksum="ec7ca4263769593df4d909be85b94a6b79efa2897be5d2bb8ebd516e823175af"
        osv_asset="osv-scanner_darwin_arm64"
        osv_checksum="a8cd6507b06239f463a7642430cfd2d154882f150f6e30cdc0653e28dfc34216"
        ;;
    *)
        echo "Security audit tools are not pinned for $(uname -s)/$(uname -m)." >&2
        exit 1
        ;;
esac
readonly cargo_asset cargo_checksum osv_asset osv_checksum
readonly cargo_archive="${cargo_audit_dir}/${cargo_asset}"
readonly cargo_extract="${cargo_audit_dir}/cargo-audit.extract"

checksum_matches() {
    local expected="$1"
    local path="$2"
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s  %s\n' "${expected}" "${path}" | sha256sum --check --status
    elif command -v shasum >/dev/null 2>&1; then
        [[ "$(shasum -a 256 "${path}" | awk '{print $1}')" == "${expected}" ]]
    else
        echo "sha256sum or shasum is required to verify security tools." >&2
        return 1
    fi
}

download_verified() {
    local expected="$1"
    local url="$2"
    local max_bytes="$3"
    local destination="$4"
    local partial="${destination}.download"

    for path in "${destination}" "${partial}"; do
        if [[ -L "${path}" ]] || [[ -e "${path}" && ! -f "${path}" ]]; then
            echo "Refusing unsafe security-tool download path ${path}." >&2
            return 1
        fi
    done
    if [[ -f "${destination}" ]] && checksum_matches "${expected}" "${destination}"; then
        return 0
    fi

    curl \
        --http1.1 \
        --fail \
        --location \
        --proto '=https' \
        --proto-redir '=https' \
        --retry 2 \
        --connect-timeout 15 \
        --max-time 180 \
        --max-filesize "${max_bytes}" \
        --output "${partial}" \
        "${url}"
    if ! checksum_matches "${expected}" "${partial}"; then
        echo "Checksum verification failed for downloaded security tool." >&2
        return 1
    fi
    mv -- "${partial}" "${destination}"
}

download_verified \
    "${cargo_checksum}" \
    "https://github.com/rustsec/rustsec/releases/download/cargo-audit/v${CARGO_AUDIT_VERSION}/${cargo_asset}" \
    8388608 \
    "${cargo_archive}"

for path in "${cargo_audit_bin}" "${cargo_extract}"; do
    if [[ -L "${path}" ]] || [[ -e "${path}" && ! -f "${path}" ]]; then
        echo "Refusing unsafe cargo-audit binary path ${path}." >&2
        exit 1
    fi
done
cargo_member="${cargo_asset%.tgz}/cargo-audit"
readonly cargo_member
if [[ "$(tar -tzf "${cargo_archive}" | grep -Fxc "${cargo_member}")" != "1" ]]; then
    echo "Pinned cargo-audit archive does not contain the expected binary." >&2
    exit 1
fi
# Always derive the executable from the checksum-verified archive. A replaced
# cached executable must never be able to approve its own integrity.
tar -xOzf "${cargo_archive}" "${cargo_member}" >"${cargo_extract}"
chmod 0755 "${cargo_extract}"
mv -- "${cargo_extract}" "${cargo_audit_bin}"
cargo_audit_output="$("${cargo_audit_bin}" --version 2>/dev/null || true)"
if [[ "${cargo_audit_output##* }" != "${CARGO_AUDIT_VERSION}" ]]; then
    echo "Expected cargo-audit ${CARGO_AUDIT_VERSION}, found: ${cargo_audit_output:-unusable binary}" >&2
    exit 1
fi

today="$(date -u +%Y-%m-%d)"
readonly today

exception_is_current_and_documented() {
    local advisory="$1"
    awk -v advisory="${advisory}" -v today="${today}" '
        BEGIN {
            RS = "\\[\\[IgnoredVulns\\]\\]"
            FS = "\n"
            matches = 0
            valid = 0
        }
        {
            id = ""
            expiry = ""
            reason = ""
            for (line_number = 1; line_number <= NF; line_number += 1) {
                line = $line_number
                if (line ~ /^[[:space:]]*id[[:space:]]*=[[:space:]]*"[^\"]+"[[:space:]]*$/) {
                    id = line
                    sub(/^[^\"]*"/, "", id)
                    sub(/"[[:space:]]*$/, "", id)
                } else if (line ~ /^[[:space:]]*ignoreUntil[[:space:]]*=[[:space:]]*[0-9][0-9][0-9][0-9]-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])[[:space:]]*$/) {
                    expiry = line
                    sub(/^[^=]*=[[:space:]]*/, "", expiry)
                    sub(/[[:space:]]*$/, "", expiry)
                } else if (line ~ /^[[:space:]]*reason[[:space:]]*=[[:space:]]*"[^\"]+"[[:space:]]*$/) {
                    reason = line
                }
            }
            if (id == advisory) {
                matches += 1
                if (expiry > today && reason != "") {
                    valid += 1
                }
            }
        }
        END { exit !(matches == 1 && valid == 1) }
    ' "${osv_config}"
}

for advisory in "${rustsec_ignores[@]}"; do
    if ! exception_is_current_and_documented "${advisory}"; then
        echo "cargo-audit exception ${advisory} must have one unexpired, reasoned OSV exception." >&2
        exit 1
    fi
done
while IFS= read -r configured_advisory; do
    if ! exception_is_current_and_documented "${configured_advisory}"; then
        echo "OSV exception ${configured_advisory} must be unique, unexpired, and reasoned." >&2
        exit 1
    fi
    found=false
    for advisory in "${rustsec_ignores[@]}"; do
        if [[ "${configured_advisory}" == "${advisory}" ]]; then
            found=true
            break
        fi
    done
    if [[ "${configured_advisory}" == RUSTSEC-* && "${found}" != true ]]; then
        echo "OSV RustSec exception ${configured_advisory} is not mirrored by cargo-audit." >&2
        exit 1
    fi
done < <(sed -n 's/^[[:space:]]*id[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "${osv_config}")

# cargo-audit has no exception-expiry field. The exact same exceptions are
# required above in osv-scanner.toml, whose ignoreUntil dates OSV enforces.
cargo_audit_args=(
    --db "${rustsec_db}"
    --file "${repo_root}/Cargo.lock"
)
for advisory in "${rustsec_ignores[@]}"; do
    cargo_audit_args+=(--ignore "${advisory}")
done
CARGO_HOME="${cargo_home}" "${cargo_audit_bin}" audit "${cargo_audit_args[@]}"

download_verified \
    "${osv_checksum}" \
    "https://github.com/google/osv-scanner/releases/download/v${OSV_SCANNER_VERSION}/${osv_asset}" \
    67108864 \
    "${osv_scanner}"
chmod 0755 "${osv_scanner}"

version_output="$("${osv_scanner}" --version)"
if [[ "${version_output%%$'\n'*}" != "osv-scanner version: ${OSV_SCANNER_VERSION}" ]]; then
    echo "Pinned OSV Scanner reported an unexpected version:" >&2
    echo "${version_output}" >&2
    exit 1
fi

"${osv_scanner}" scan source \
    --config="${osv_config}" \
    --format=vertical \
    --verbosity=error \
    --lockfile="${repo_root}/Cargo.lock" \
    --lockfile="${repo_root}/vault/client/package-lock.json"
