# URI 格式

AnyTLS 的 URI 格式旨在提供一种简洁的方式来表示连接到 AnyTLS 服务器所需的必要信息。它包括各种参数，如服务器地址、验证密码，TLS 设置等。

本格式参考了 [Hysteria2](https://v2.hysteria.network/zh/docs/developers/URI-Scheme/)

## 结构

```
anytls://[auth@]hostname[:port]/?[key=value]&[key=value]...
```

## 组件

### 协议名

`anytls`

### 验证

验证密码应在 URI 的 `auth` 中指定。这部分实际上就是标准 URI 格式中的用户名部分，因此如果包含特殊字符，需要进行 [百分号编码](https://datatracker.ietf.org/doc/html/rfc3986#section-2.1)。

### 地址

服务器的地址和可选端口。如果省略端口，则默认为 443。

### 参数

- `sni`：用于 TLS 连接的服务器 SNI。（特殊情况：当 `sni` 的值为 [IP 地址](https://datatracker.ietf.org/doc/html/rfc6066#section-3:~:text=Literal%20IPv4%20and%20IPv6%20addresses%20are%20not%20permitted%20in%20%22HostName%22.)时，客户端必须不发送 SNI）

- `insecure`：是否允许不安全的 TLS 连接。接受 `1` 表示 `true`，`0` 表示 `false`。

### 片段 (Fragment)

URI 中 `?` 参数列表之后的 `#` 部分称为片段（Fragment）。在 AnyTLS 中，**片段用于指定节点的显示名称**。
- 片段内容必须使用 [URL 编码（百分号编码）](https://datatracker.ietf.org/doc/html/rfc3986#section-2.1) 处理，以确保空格和特殊字符能被正确解析与传输。
- 客户端在解析 URI 时，应对片段进行 URL 解码以获取可读的节点名称/标签。
- 示例：`#my%20tag%20with%20spaces` 解码后为 `my tag with spaces`。

## 示例

```
anytls://letmein@example.com/?sni=real.example.com
anytls://letmein@example.com/?sni=127.0.0.1&insecure=1
anytls://0fdf77d7-d4ba-455e-9ed9-a98dd6d5489a@[2409:8a71:6a00:1953::615]:8964/?insecure=1
```

## 注意事项

这个 URI 故意只包含连接到 AnyTLS 服务器所需的基础信息。尽管第三方实现可以根据需要添加额外的参数，但它们不应假设其他实现能理解这些额外参数。
