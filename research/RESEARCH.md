# DepotManager 协议调研笔记（基于 SteamRE/DepotDownloader + SteamRE/SteamKit 源码）

## CM 连接（WebSocket，无加密握手）
- 服务器列表: `GET https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/?cellid=0` → `response.serverlist[] = {endpoint:"host:port", type:"websockets"}`
- 连接 `wss://{endpoint}/cmsocket/`，二进制帧，**一帧 = 一条完整消息**，全部明文 protobuf（TLS 提供安全）。
- **WS 模式不做 ChannelEncrypt 握手**（仅 TCP/UDP 需要 AES+HMAC）。服务器不会主动发言，客户端须先发 ClientHello。
- 帧格式 (MsgHdrProtoBuf): `u32 emsg|0x80000000` `i32 header_len` `CMsgProtoBufHeader` `body`。EMsgMask=0x3FFFFFFF, ProtoMask=0x80000000。
- 非 proto 帧（仅 ChannelEncrypt*=1303/1304/1305 用 MsgHdr: `u32 emsg` `u64 target_job` `u64 source_job`）。WS 下用不到。

## 关键 EMsg
- Multi=1, ServiceMethodResponse=147, ServiceMethodCallFromClient=151, ServiceMethodCallFromClientNonAuthed=9804
- ClientHeartBeat=703, ClientLogOff=706, ClientLogOnResponse=751, ClientLoggedOff=757, ClientLicenseList=780, ClientSessionToken=850
- ChannelEncryptRequest/Response/Result = 1303/1304/1305
- ClientGetDepotDecryptionKey=5438, Response=5439; ClientLogon=5514; ClientRequestFreeLicense=5572/5573
- ClientPICSChangesSinceRequest=8901, ClientPICSProductInfoRequest=8903/Response=8904, ClientPICSAccessTokenRequest=8905/Response=8906
- ClientServerUnavailable=5500; ClientServerTimestampRequest=9802/Response=9803; ClientHello=9805

## 登录
- ClientHello: `{protocol_version: 65581}`
- CMsgClientLogon 关键字段: protocol_version=65581, client_package_version=1771, client_os_type(Win11=20, Win10=16), client_language="english", cell_id, machine_id, supports_rate_limit_response=true; 匿名: 无 account_name；账号: access_token=refresh_token（推荐）或 account_name+password(+auth_code/two_factor_code)
- 匿名 header steamid = (1<<56)|(10<<52)|(0<<32)|0 = 0x01A0000000000000；Individual = (1<<56)|(1<<52)|(1<<32)|accountid
- CMsgClientLogonResponse: eresult(OK=1), cell_id, heartbeat_seconds, header.steamid/client_sessionid（之后所有消息带上）
- EResult: OK=1 Fail=2 NoConnection=3 InvalidPassword=5 InvalidParam=8 FileNotFound=9 AccessDenied=15 AccountNotFound=18 ServiceUnavailable=20 Pending=22 Revoked=26 Expired=27 Blocked=40 AccountLocked=44 TryAnotherCM=48 AccountLogonDenied=63 InvalidLoginAuthCode=65 RateLimitExceeded=84 AccountLoginDeniedNeedTwoFactor=85 TwoFactorCodeMismatch=88 LimitedUserAccount=112
- 心跳: 每 heartbeat_seconds 发 ClientHeartBeat(703) 空 proto
- 登录后服务器推 ClientLicenseList(780): licenses[]{package_id, access_token}
- machine_id: 二进制KV `00 "MessageObject" 00 | 01 "BB3" 00 hex(sha1(machineGuid)) 00 | 01 "FF2" 00 hex(sha1(mac16B)) 00 | 01 "3B3" 00 hex(sha1(diskId)) 00 | 08 | 08`（用持久化随机值即可）
- 账号认证（新版认证服务, unified messages）:
  1. `Authentication.GetPasswordRSAPublicKey#1` {account_name} → {publickey_mod(hex), publickey_exp(hex), timestamp}
  2. RSA/PKCS1 加密密码 → base64；`Authentication.BeginAuthSessionViaCredentials#1` {account_name, encrypted_password(b64), encryption_timestamp, persistence(1=Persistent), device_friendly_name, device_details{platform_type=1(SteamClient), os_type, device_friendly_name}, guard_data?}
  3. QR: `Authentication.BeginAuthSessionViaQR#1` {device_friendly_name, device_details} → {client_id, challenge_url, request_id, interval, allowed_confirmations[]}
  4. 轮询 `Authentication.PollAuthSessionStatus#1` {client_id, request_id} → refresh_token+access_token+account_name+new_guard_data（refresh_token 用于 LogOn.access_token）
  5. Guard 码: `Authentication.UpdateAuthSessionWithSteamGuardCode#1` {client_id, steamid, code, code_type(2=EmailCode,3=DeviceCode)}
  - EAuthSessionGuardType: None=1 EmailCode=2 DeviceCode=3 DeviceConfirmation=4 EmailConfirmation=5 MachineToken=6
- Unified 调用: header.target_job_name = "Service.Method#1"，EMsg=151(已登录)/9804(未登录)；响应 EMsg=147, jobid_target=请求 jobid_source, header.eresult

## PICS（产品信息）
- 访问令牌: ClientPICSAccessTokenRequest{appids[]} → Response{app_access_tokens[{appid, access_token}], app_denied_tokens[]}
- 产品信息: ClientPICSProductInfoRequest{apps[{appid, access_token}]} → **可能多条** ClientPICSProductInfoResponse 直到 response_pending==false；apps[{appid, change_number, sha, buffer, missing_token, size}]
- **app buffer = 文本 VDF**（去掉末尾 1 字节 \0）；package buffer = u32 版本 + 二进制 KV
- appinfo 结构: common{name, FreeToDownload}, depots{<depotId>{config{oslist,osarch,language,lowviolence}, manifests{<branch>{gid}}, depotfromapp}, branches{<name>{buildid, description?}}, workshopdepot}
- 匿名访问检查: package 17906 的 appids/depotids；或 appinfo common.FreeToDownload=true
- 免费游戏授权: ClientRequestFreeLicense{app_ids[]} → granted_appids/granted_packageids（匿名不可用）

## CDN 下载
- 服务器: unified `ContentServerDirectory.GetServersForSteamPipe#1` {cell_id} → servers[]{type, host, vhost, https_support("mandatory"→https:443 否则 http:80), allowed_app_ids, weighted_load, num_entries_in_client_list, use_as_proxy, steam_china_only}
  - 过滤: type ∈ {"SteamCache","CDN"} 且 (allowed_app_ids 空 或 含 appId)
- Manifest 请求码: `ContentServerDirectory.GetManifestRequestCode#1` {app_id, depot_id, manifest_id, app_branch(public→null)} → manifest_request_code（可能 0=不允许下载旧版/需登录）
- CDN auth token（403 时）: `ContentServerDirectory.GetCDNAuthToken#1` {depot_id, host_name, app_id} → {token, expiration_time}
- Depot 密钥: ClientGetDepotDecryptionKey(5438){depot_id, app_id} → {eresult, depot_key(32B)}
- Manifest URL: `GET {scheme}://{vhost}:{port}/depot/{depotId}/manifest/{gid}/5/{requestcode}[?{cdntoken}]` → **ZIP**（单条目）内含二进制 manifest
- Chunk URL: `GET {scheme}://{vhost}:{port}/depot/{depotId}/chunk/{chunkIdHexLower}[?{cdntoken}]`
- Manifest 二进制: 区段 `magic u32 LE + len u32 + protobuf`；magic: payload=0x71F617D0, metadata=0x1F4812BE, signature=0x1B81B817, end=0x32C415AB
  - ContentManifestPayload.mappings[]: filename, size, flags, sha_filename, sha_content, chunks[]{sha(20B id), crc(adler32), offset, cb_original, cb_compressed}, linktarget
  - Metadata: depot_id, gid_manifest, creation_time, filenames_encrypted, cb_disk_original, cb_disk_compressed
  - 文件名解密: base64 → [16B AES-256-ECB→IV][AES-256-CBC PKCS7] → UTF8 去尾 \0；EDepotFileFlag: Executable=32 Directory=64 Symlink=512...
- Chunk 解密解压: [16B ECB(depotKey)→IV][CBC PKCS7] → 压缩数据：
  - "VSZa"(0x615A5356): vZstd: hdr 8B(magic u32+crc u32) + zstd帧 + footer 15B(crc u32+size u32+"zsv")
  - "VZa"(0x5A56 'a'): VZip LZMA: magic u16 'VZ' + 'a' + crc/timestamp u32 + lzma属性5B(lc/lp/pb + dict u32) + lzma流 + footer(crc u32 + size u32 + 0x767A 'zv')
  - "PK\x03\x04": zip deflate 单条目
  - 校验: adler32(seed 0, 解压后数据) == chunk.crc
- 文件校验: 每 chunk 读本地 offset 处 uncompressed 长度，adler32 对比

## 搜索与历史版本
- 搜索: `https://store.steampowered.com/api/storesearch/?term=X&cc=us&l=en` → {items:[{id,name,tiny_image,price,...}]}（本机网络被墙，属环境问题；实现多源回退）
- 应用列表: `https://api.steampowered.com/ISteamApps/GetAppList/v2/`（本机 404；做缓存+回退）
- 历史版本: Steam 官方不公开；SteamDB 跟踪。来源: ①SteamDB `/depot/{depotId}/manifests/` HTML 抓取（易被 Cloudflare 403）②PICS 分支列表（public+各 beta 的当前 manifest gid/buildid，始终可用）③手动粘贴 manifest gid
- 旧 manifest 需要 GetManifestRequestCode（账号登录成功率高；开发者可禁用旧版下载）

## 本机网络实测
- api.steampowered.com (CM列表) ✔; store.steampowered.com ✘(超时); GetAppList ✘(404); steamdb.info ✘(403/410); CM WebSocket ✔ 可连（须先发 ClientHello）; CM TCP 27017-27019 ✘
