#!/bin/bash

#==========================================================
#   System Request: Debian 7+ / Ubuntu 22.04+
#   Author: ssrlive
#   Description: AnyTLS onekey support for pure IP or domain certificate and sspanel integration
#   Version: 1.0.0
#
# Usage:
#   ./anytls-install-2026.sh install [--use-sspanel]
#   ./anytls-install-2026.sh uninstall
#
#   --use-sspanel  # enable sspanel integration, default: no integration
#==========================================================

#fonts color
Green="\033[32m"
Red="\033[31m"
Yellow="\033[33m"
GreenBG="\033[42;37m"
RedBG="\033[41;37m"
ColorEnd="\033[0m"

#notification information
Info="${Green}[Info]${ColorEnd}"
OK="${Green}[OK]${ColorEnd}"
Error="${Red}[Error]${ColorEnd}"

function get_binary_target() {
    local _binary_target=""
    local CPU_ARCH=`uname -m`
    case ${CPU_ARCH} in
        x86_64)
            _binary_target="x86_64-unknown-linux-musl"
            ;;
        aarch64)
            _binary_target="aarch64-unknown-linux-musl"
            ;;
        armv7l)
            _binary_target="armv7-unknown-linux-musleabihf"
            ;;
        *)
            echo -e "${Error} ${RedBG} The current CPU architecture ${CPU_ARCH} is not supported. Please contact the author! ${ColorEnd}"
            exit 1
            ;;
    esac
    echo ${_binary_target}
}

cpu_arch_target=$(get_binary_target)

# anytls_install_sh="anytls-install-2026.sh"
# anytls_install_sh_url="https://github.com/ssrlive/anytls-rs/raw/refs/heads/master/install/anytls-install-2026.sh"

server_bin_download_url="https://github.com/ssrlive/anytls-rs/releases/latest/download/anytls-${cpu_arch_target}.zip"

service_name=anytls-server-2026
service_unit_file_path=/etc/systemd/system/${service_name}.service

target_bin_path="/usr/local/bin/${service_name}"
web_svr_domain=""
svr_listen_port=443
web_svr_public_ip_addr=""
letsencrypt_cert_file=""
letsencrypt_key_file=""
sspanel_node_id=""
sspanel_server_url=""
sspanel_api_token=""
use_sspanel="false"

function random_string_gen() {
    local PASS=""
    local MATRIX="0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    local LENGTH=$1
    [ -z $1 ] && LENGTH="16"
    while [ "${n:=1}" -le "$LENGTH" ]
    do
        PASS="$PASS${MATRIX:$(($RANDOM%${#MATRIX})):1}"
        let n+=1
    done

    echo ${PASS}
}

# Reverse proxy entry point.
export password=$(random_string_gen 20)

function check_root_account() {
    if [ `id -u` == 0 ]; then
        echo -e "${OK} ${GreenBG} Current account is the root user, enter the installation process ${ColorEnd} "
        sleep 3
    else
        echo -e "${Error} ${RedBG} Current account is not root user, please switch to the root user and re-execute this script ${ColorEnd}"
        exit 1
    fi
}

source /etc/os-release

# Extract the English name of the distribution system from VERSION, in order to add the corresponding nginx apt source under debian / ubuntu
VERSION=`echo ${VERSION} | awk -F "[()]" '{print $2}'`

function script_file_full_path() {
    echo $(readlink -f "$0")
}

function judge() {
    if [[ $? -eq 0 ]]; then
        echo -e "${OK} ${GreenBG} $1 Completed ${ColorEnd}"
        sleep 1
    else
        echo -e "${Error} ${RedBG} $1 Failed ${ColorEnd}"
        exit 1
    fi
}

function dependency_install() {
    apt update -y
    apt install qrencode curl wget git lsof nginx-extras cron bc unzip vim autoconf libtool openssl libssl-dev -y
    if [[ "${ID}" == "ubuntu" && `echo "${VERSION_ID}" | cut -d '.' -f1` -ge 20 ]]; then
        apt install inetutils-ping -y
    fi

    judge "Installing dependencies"
}

function is_port_available() {
    local the_port="$1"

    # Check if port is already listening on TCP
    if lsof -iTCP:"${the_port}" -sTCP:LISTEN -Pn >/dev/null 2>&1; then
        return 1
    fi

    return 0
}

function random_listen_port() {
    local the_port=0
    while true; do
        the_port=$(shuf -i 9000-19999 -n 1)
        expr ${the_port} + 1 &>/dev/null
        if [ $? -eq 0 ]; then
            if [ ${the_port} -ge 1 ] && [ ${the_port} -le 65535 ] && [ ${the_port:0:1} != 0 ]; then
                if is_port_available "${the_port}"; then
                    break
                fi
            fi
        fi
    done
    echo ${the_port}
}

function check_file_exists() {
    local file_path="${1}"

    if [ ! -f "${file_path}" ]; then
        echo -e "${RedBG} Error: ${file_path} not found. ${ColorEnd}"
        exit 1
    fi
}

function get_vps_valid_ip() {
    local web_svr_local_ip_v4_addr=`curl -4 ip.sb 2>/dev/null`
    local web_svr_local_ip_v6_addr=`curl -6 ip.sb 2>/dev/null`
    local ip_v4_regex='^(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$'
    local ip_v6_regex='^([0-9a-fA-F]{0,4}:){1,7}[0-9a-fA-F]{0,4}$'
    if [[ $web_svr_local_ip_v4_addr =~ $ip_v4_regex ]]; then
        echo -e "${web_svr_local_ip_v4_addr}"
        return 0
    elif [[ $web_svr_local_ip_v6_addr =~ $ip_v6_regex ]]; then
        echo -e "${web_svr_local_ip_v6_addr}"
        return 0
    else
        echo -e "${RedBG} No valid IP found. ${ColorEnd}"
        return 1
    fi
}

function download_n_install_the_server_bin() {
    local target_bin_zip_file="local_tmp.zip"
    local local_target_bin_path="${1}"
    local target_bin_name="anytls-server"

    rm -rf ${target_bin_zip_file}
    curl -L ${server_bin_download_url} -o ${target_bin_zip_file} >/dev/null 2>&1
    if [ $? -ne 0 ]; then echo "curl failed"; exit -1; fi

    rm -rf ${target_bin_name}
    unzip ${target_bin_zip_file} ${target_bin_name} >/dev/null 2>&1
    if [ $? -ne 0 ]; then echo "unzip failed"; exit -1; fi

    chmod +x ${target_bin_name}
    rm -rf ${target_bin_zip_file}

    rm -rf ${local_target_bin_path}
    local target_dir="$(dirname "${local_target_bin_path}")"
    mkdir -p "${target_dir}"
    mv ${target_bin_name} ${local_target_bin_path}

    echo "${local_target_bin_path}"
}

function write_service_unit_file() {
    local svc_name=${1}
    local svc_exec_command=${2}
    local _service_unit_file_path="${3}"

    cat > "${_service_unit_file_path}" <<-EOF
[Unit]
    Description=${svc_name}
    After=network.target
[Service]
    Type=simple
    ExecStart=${svc_exec_command}
    PrivateTmp=true
    Restart=on-failure
    RestartSec=35s
    LimitNOFILE=1000000
    LimitCORE=infinity
[Install]
    WantedBy=multi-user.target
EOF

    chmod 754 "${_service_unit_file_path}"
}

function create_the_server_systemd_service() {
    local command_line="${1}"

    local work_dir="$(dirname $(script_file_full_path))"

    ldconfig
    cd "${work_dir}"

    write_service_unit_file "${service_name}" "${command_line}" "${service_unit_file_path}"

    echo "${service_name} starting..."

    systemctl daemon-reload
    systemctl enable ${service_name}.service
    sleep 2

    # FIXME: If running script with `service` parameter, this line will failed and cause the script to exit abnormally.
    systemctl start ${service_name}.service
    sleep 2
}

function request_host_or_ip_cert() {
    local host_or_ip=${web_svr_domain}
    local cert_script_url="https://github.com/ssrlive/tips/raw/refs/heads/master/tips/pure-ip-cert.sh"

    curl -L ${cert_script_url} -o pure-ip-cert.sh 2>/dev/null
    if [ $? -ne 0 ]; then
        echo -e "${Error} ${RedBG} Failed to download pure-ip-cert.sh script. Please check your network connection or download it manually from ${cert_script_url} ${ColorEnd}"
        exit 1
    fi
    chmod +x pure-ip-cert.sh
    bash ./pure-ip-cert.sh ${host_or_ip}
    if [ $? -ne 0 ]; then
        echo -e "${Error} ${RedBG} Certificate generation failed. ${ColorEnd}"
        exit 1
    fi

    # 证书的各种文件存储在 ~/.acme.sh/${host_or_ip}_ecc 目录下， 其中 ${host_or_ip} 是你当前主机的公网 IP 或 域名。
    letsencrypt_cert_file="${HOME}/.acme.sh/${host_or_ip}_ecc/fullchain.cer"
    letsencrypt_key_file="${HOME}/.acme.sh/${host_or_ip}_ecc/${host_or_ip}.key"
}

function do_uninstall_service_action() {
    ldconfig

    sleep 2

    systemctl stop ${service_name}.service
    sleep 2

    systemctl disable ${service_name}.service 2>/dev/null
    echo -e "${Info} ${service_name} service stopped and disabled."

    rm -rf ${target_bin_path}
    echo -e "${Info} ${service_name} binary removed from ${target_bin_path}."

    rm -rf ${service_unit_file_path}
    echo -e "${Info} ${service_name} systemd service unit file removed from ${service_unit_file_path}."

    crontab -l 2>/dev/null | grep -vF "${service_name}.service" | crontab -
    echo -e "${Info} ${service_name} cron job removed."

    systemctl daemon-reload

    echo -e "${Info} ${GreenBG} ${service_name} uninstall success! ${ColorEnd}"
}

# Uninstall service and clean up files, but keep the certificate files for future use.
function uninstall_service() {
    printf "Are you sure uninstall ${service_name}? (y/n)\n"
    read -p "(Default: n):" answer
    [ -z ${answer} ] && answer="n"
    if [ "${answer}" == "y" ] || [ "${answer}" == "Y" ]; then
        do_uninstall_service_action
    else
        echo
        echo "uninstall cancelled, nothing to do..."
        echo
    fi
}

function print_url() {
    local command_line="${1}"

    local qrcode="$( ${command_line} --print-url )"
    echo
    echo "${qrcode}"
    echo
    echo

    qrencode -t UTF8 "${qrcode}" | cat
}

function cron_random_restart_service() {
    local random_hour=$(od -An -N1 -i /dev/urandom | awk '{print $1 % 24}')
    local random_minute=$(od -An -N1 -i /dev/urandom | awk '{print $1 % 60}')
    local restart_job_2026="${random_minute} ${random_hour} * * * systemctl restart ${service_name}.service"

    if crontab -l 2>/dev/null | grep -Fq "systemctl restart ${service_name}.service"; then
        echo -e "${OK} ${GreenBG} ${service_name} restart cron job already exists, skipping add. ${ColorEnd}"
        return 0
    fi

    (crontab -l 2>/dev/null; echo "${restart_job_2026}") | crontab -
}

function collect_the_server_info() {
    echo ""
    echo -e "${Info} ${GreenBG} ==== Now input some node server information ==== ${ColorEnd} "

    web_svr_public_ip_addr=$(get_vps_valid_ip)
    local exit_status=$?
    if [[ $exit_status -ne 0 ]]; then
        echo -e "${Error} ${RedBG} No valid IP found. ${ColorEnd}"
        exit 1
    fi

    echo ""
    echo "请输入 你的节点域名 (形如 mygooodsite.com), 如果不想输入域名,可直接回车跳过,这将使用纯 IP (${web_svr_public_ip_addr}) 证书"
    echo "Please enter your node domain name (for example: mygooodsite.com), if you don't want to enter a domain,"
    stty erase '^H' && read -p "press Enter to skip, thus a pure IP (${web_svr_public_ip_addr}) certificate will be used: " domain_name
    [[ -z ${domain_name} ]] && domain_name=${web_svr_public_ip_addr}
    web_svr_domain=${domain_name}

    echo ""
    svr_listen_port=`random_listen_port`
    echo "请输入 节点端口号 (默认值 ${svr_listen_port})"
    stty erase '^H' && read -p "Please enter the node port number (default: ${svr_listen_port}): " port
    [[ -z ${port} ]] && port=${svr_listen_port}
    svr_listen_port=${port}

    echo ""
    echo "请输入 通讯密码, 默认值 ${password} "
    stty erase '^H' && read -p "Please enter communication password (default ${password}): " rvs_path
    [[ -z ${rvs_path} ]] && rvs_path=${password}
    password=${rvs_path}

    echo ""
    if [[ "${use_sspanel}" == "true" ]]; then
        echo -e "${Info} ${GreenBG} ==== sspanel integration enabled ==== ${ColorEnd} "
        echo ""
        echo -e "${Info} ${GreenBG} ==== Now input sspanel related information ==== ${ColorEnd} "

        echo ""
        echo "请输入 sspanel 面板内为 本节点服务端 生成的节点 ID (形如 1)"
        stty erase '^H' && read -p "Please enter node ID generated in sspanel (for example: 1): " sspanel_node_id
        if [[ -z ${sspanel_node_id} ]]; then
            echo -e "${Error} ${RedBG} Node ID cannot be empty! ${ColorEnd}"
            exit 1
        fi

        echo ""
        echo "请输入 sspanel 服务器的 API 地址 (形如 https://mysspanel.com 或 https://mysspanel.com:6543), 如果你面板是 https 协议且使用了非标准端口（非 443 端口）,请务必在地址中包含端口号"
        stty erase '^H' && read -p "Please enter sspanel server address (for example: https://mysspanel.com or https://mysspanel.com:6543): " sspanel_server_url
        if [[ -z ${sspanel_server_url} ]]; then
            echo -e "${Error} ${RedBG} sspanel server address cannot be empty! ${ColorEnd}"
            exit 1
        fi

        echo ""
        echo "请输入 sspanel 面板内的 API Token (形如 1234567890abcdef)"
        stty erase '^H' && read -p "Please enter API Token (for example: 1234567890abcdef): " sspanel_api_token
        if [[ -z ${sspanel_api_token} ]]; then
            echo -e "${Error} ${RedBG} API Token cannot be empty! ${ColorEnd}"
            exit 1
        fi
    else
        echo -e "${Info} ${GreenBG} ==== sspanel integration disabled (default), skipping sspanel prompts ==== ${ColorEnd} "
        echo -e "${Info} To enable sspanel integration, run with --use-sspanel parameter."
    fi

    echo ""
    echo -e "${Info} ${GreenBG} ==== Now all information has been collected, starting installation ==== ${ColorEnd} "
    echo ""
}

function install_the_remote_server() {
    dependency_install
    collect_the_server_info

    do_uninstall_service_action

    request_host_or_ip_cert

    local svc_bin_path=$(download_n_install_the_server_bin "${target_bin_path}")
    echo -e "${OK} ${GreenBG} ${service_name} binary installed at ${svc_bin_path} ${ColorEnd}"

    if ! [ -f "${svc_bin_path}" ]; then
        echo -e "${Error} ${RedBG} ${service_name} install failed, please contact the author! ${ColorEnd}"
        exit 1
    fi

    if [[ "${use_sspanel}" == "true" ]]; then
        local command_line="${target_bin_path} -l 0.0.0.0:${svr_listen_port} -p ${password} --forward http://127.0.0.1 --cert ${letsencrypt_cert_file} --key ${letsencrypt_key_file} --sni ${web_svr_domain} --panel-webapi-url ${sspanel_server_url} --panel-webapi-token ${sspanel_api_token} --panel-node-id ${sspanel_node_id}"
    else
        local command_line="${target_bin_path} -l 0.0.0.0:${svr_listen_port} -p ${password} --forward http://127.0.0.1 --cert ${letsencrypt_cert_file} --key ${letsencrypt_key_file} --sni ${web_svr_domain}"
    fi

    create_the_server_systemd_service "${command_line}"

    cron_random_restart_service

    if [[ "${use_sspanel}" == "true" ]]; then
        echo
        echo "======== config ========"
        echo
        ${command_line} --print-args
        echo
        echo "============================="
        echo

        echo -e "${OK} ${Green} ${service_name} installed successfully with sspanel integration! ${ColorEnd}"
        echo ""
        echo -e "${Info} ${Green} 请将上面 分隔线内的 配置 内容复制到 sspanel 面板内的 本节点服务端 的 自定义配置 编辑框中 ${ColorEnd}"
        echo -e "${Info} ${Green} Please copy the config content between the lines above into the custom configuration box of the node service in sspanel. ${ColorEnd}"
    else
        print_url "${command_line}"
    fi
    echo ""
}

function main() {
    echo
    echo "####################################################################"
    echo "# Script of Install ${service_name} Server with pure IP or domain certificate"
    echo "# Author: ssrlive"
    echo "# Github: https://github.com/ssrlive"
    echo "####################################################################"
    echo

    local action=${1}
    shift
    [ -z "${action}" ] && action="install"
    case "${action}" in
        install)
            if [[ "${1}" == "--use-sspanel" ]]; then
                use_sspanel="true"
            fi
            check_root_account
            install_the_remote_server
            ;;
        uninstall)
            check_root_account
            uninstall_service
            ;;
        *)
            echo "Arguments error! [${action}]"
            echo "Usage: $(basename "$0") install [--use-sspanel]"
            echo "       $(basename "$0") uninstall"
            echo
            echo "Example: $(basename "$0") install --use-sspanel"
            ;;
    esac

    exit 0
}

main "$@"
