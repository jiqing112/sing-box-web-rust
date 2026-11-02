// 关键逻辑单元测试：订阅解析、双内核渲染、引擎差异、nft 脚本生成。

#[cfg(test)]
mod tests {
    use crate::config::RenderOpts;
    use crate::model::{normalize_outbound_type, Node};
    use serde_json::{json, Value};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn node(tag: &str, outbound: Value) -> Node {
        serde_json::from_value(json!({
            "id": format!("id-{tag}"),
            "tag": tag,
            "name": tag,
            "type": outbound.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            "server": outbound.get("server").and_then(|v| v.as_str()).unwrap_or(""),
            "port": outbound.get("server_port").and_then(|v| v.as_i64()).unwrap_or(0),
            "region": "",
            "sub_id": "s1",
            "enabled": true,
            "outbound": outbound,
        }))
        .unwrap()
    }

    fn opts(eng: &str) -> RenderOpts {
        RenderOpts {
            engine: eng.into(),
            data_dir: PathBuf::from("/tmp/data"),
            mixed_port: 7890,
            mixed_listen: "127.0.0.1".into(),
            tproxy_port: 12345,
            clash_addr: "127.0.0.1:9091".into(),
            ..Default::default()
        }
    }

    #[test]
    fn normalize_types() {
        assert_eq!(normalize_outbound_type("ss"), "shadowsocks");
        assert_eq!(normalize_outbound_type("hy2"), "hysteria2");
        assert_eq!(normalize_outbound_type("socks5"), "socks");
        assert_eq!(normalize_outbound_type("vless"), "vless");
    }

    #[test]
    fn parse_uri_list_base64_and_schemes() {
        let body = "vless://b831381d-6324-4d53-ad4f-8cda48b30812@example.com:443?security=reality&pbk=pk1&sid=ab&fp=chrome&type=ws&path=%2Fws&host=h.com#%F0%9F%87%AD%F0%9F%87%B0%20%E9%A6%99%E6%B8%AF%2001\n\
                    trojan://pass@example.com:8443?sni=s.com#TR1\n";
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(body);
        let nodes = crate::subscription::parse::parse(b64.as_bytes(), "s1", &[], &mut HashSet::new()).unwrap();
        assert_eq!(nodes.len(), 2);
        let n = &nodes[0];
        assert_eq!(n.name, "🇭🇰 香港 01");
        assert_eq!(n.region, "HK");
        assert_eq!(n.outbound["type"], "vless");
        assert_eq!(n.outbound["tls"]["reality"]["public_key"], "pk1");
        assert_eq!(n.outbound["transport"]["path"], "/ws");
        assert_eq!(nodes[1].outbound["type"], "trojan");
        assert_eq!(nodes[1].outbound["password"], "pass");
    }

    #[test]
    fn parse_clash_yaml_with_info_filter() {
        let body = r#"
proxies:
  - {name: "剩余流量：1 GB", type: ss, server: a.com, port: 1, cipher: aes-128-gcm, password: x}
  - {name: "🇯🇵 JP", type: ss, server: 127.0.0.1, port: 8388, cipher: aes-128-gcm, password: p}
"#
        .as_bytes();
        let nodes = crate::subscription::parse::parse(body, "s1", &[], &mut HashSet::new()).unwrap();
        assert_eq!(nodes.len(), 1, "info node must be filtered");
        assert_eq!(nodes[0].outbound["type"], "ss");
        assert_eq!(nodes[0].outbound["method"], "aes-128-gcm");
    }

    #[test]
    fn tag_dedup_across_refresh() {
        let body = b"vless://uuid@example.com:443#Same\nvless://uuid@example.com:444#Same\n";
        let mut reserved = HashSet::new();
        let nodes = crate::subscription::parse::parse(body, "s1", &[], &mut reserved).unwrap();
        assert_eq!(nodes[0].tag, "Same");
        assert_eq!(nodes[1].tag, "Same #2");
    }

    #[test]
    fn render_sing_box_groups_and_rule_set_outbound() {
        let tpl = crate::store::default_template();
        let nodes = vec![
            node(
                "n1",
                json!({"type":"ss","server":"1.2.3.4","server_port":8388,"method":"aes-128-gcm","password":"p"}),
            ),
            node(
                "n2",
                json!({"type":"vless","server":"1.2.3.5","server_port":443,"uuid":"u","tls":{"enabled":true,"server_name":"s.com","reality":{"enabled":true,"public_key":"pk","short_id":"si"}}}),
            ),
        ];
        let raw = crate::config::render(tpl, &nodes, &opts("sing-box")).unwrap();
        let cfg: Value = serde_json::from_slice(&raw).unwrap();
        let obs = cfg["outbounds"].as_array().unwrap();
        assert_eq!(obs[0]["type"], "selector");
        assert_eq!(obs[0]["tag"], "proxy");
        assert_eq!(obs[0]["outbounds"][0], "auto");
        assert_eq!(obs[1]["type"], "urltest");
        assert_eq!(obs[1]["tag"], "auto");
        assert!(obs.iter().any(|o| o["tag"] == "rule-set" && o["tcp_fast_open"] == true));
        assert!(obs.iter().any(|o| o["tag"] == "direct" && o["type"] == "direct"));
        // ss 的 type 必须被归一
        assert!(obs.iter().any(|o| o["tag"] == "n1" && o["type"] == "shadowsocks"));
        // vless reality 透传
        let n2 = obs.iter().find(|o| o["tag"] == "n2").unwrap();
        assert_eq!(n2["tls"]["reality"]["public_key"], "pk");
        // controller 与面板一致
        assert_eq!(cfg["experimental"]["clash_api"]["external_controller"], "127.0.0.1:9091");
    }

    #[test]
    fn render_clash_translation() {
        let tpl: Value = serde_yaml::from_str(crate::store::default_clash_template()).unwrap();
        let nodes = vec![
            node(
                "n1",
                json!({"type":"ss","server":"1.2.3.4","server_port":8388,"method":"aes-128-gcm","password":"p"}),
            ),
            node(
                "n2",
                json!({"type":"vless","server":"1.2.3.5","server_port":443,"uuid":"u","tls":{"enabled":true,"server_name":"s.com","reality":{"enabled":true,"public_key":"pk","short_id":"si"}},"transport":{"type":"ws","path":"/ws","headers":{"Host":"h.com"}}}),
            ),
        ];
        let raw = crate::config::render(tpl, &nodes, &opts("mihomo")).unwrap();
        let cfg: Value = serde_yaml::from_slice(&raw).unwrap();
        let proxies = cfg["proxies"].as_array().unwrap();
        let p1 = proxies.iter().find(|p| p["name"] == "n1").unwrap();
        assert_eq!(p1["type"], "ss");
        assert_eq!(p1["cipher"], "aes-128-gcm");
        let p2 = proxies.iter().find(|p| p["name"] == "n2").unwrap();
        assert_eq!(p2["network"], "ws");
        assert_eq!(p2["reality-opts"]["public-key"], "pk");
        assert_eq!(p2["client-fingerprint"], "chrome");
        // 策略组名与 sing-box 对齐
        let groups = cfg["proxy-groups"].as_array().unwrap();
        assert_eq!(groups[0]["name"], "proxy");
        assert_eq!(groups[1]["name"], "auto");
        assert_eq!(groups[0]["proxies"][0], "auto");
        assert_eq!(groups[0]["proxies"][1], "DIRECT");
        assert_eq!(cfg["external-controller"], "127.0.0.1:9091");
        // Windows 上不能有 listeners（redir 只在 Linux）；此测试可能在 Linux 跑
        if !cfg!(target_os = "linux") {
            assert!(cfg.get("listeners").is_none());
        } else {
            let ls = cfg["listeners"].as_array().unwrap();
            assert!(ls.iter().any(|l| l["type"] == "redir" && l["port"] == 12345));
        }
        // Linux 上必须注入/保留 :5354（nft 劫持目标）；其它平台跟随模板原样
        if cfg!(target_os = "linux") {
            assert!(cfg["dns"]["listen"].as_str().unwrap_or("").ends_with(":5354"));
        }
    }

    #[test]
    fn engine_args() {
        assert_eq!(crate::engine::run_args("sing-box", std::path::Path::new("c.json"), std::path::Path::new(".")),
                   vec!["run", "-c", "c.json"]);
        assert_eq!(crate::engine::run_args("mihomo", std::path::Path::new("c.yaml"), std::path::Path::new("/h")),
                   vec!["-d", "/h", "-f", "c.yaml"]);
        assert_eq!(crate::engine::short_version("sing-box", "sing-box version 1.14.3"), "1.14.3");
        assert_eq!(crate::engine::short_version("mihomo", "Mihomo Meta v1.19.33 linux amd64 with go1.26"), "v1.19.33");
        assert_eq!(crate::engine::normalize("meta"), "mihomo");
        assert_eq!(crate::engine::normalize(""), "sing-box");
    }

    #[test]
    fn inspect_docs() {
        let sj = br#"{"inbounds":[{"type":"mixed","listen":"127.0.0.1","listen_port":7890},{"type":"redirect","listen":"0.0.0.0","listen_port":12345}],"route":{"rules":[{"action":"sniff"}]}}"#;
        assert_eq!(crate::config::inspect::mixed_port(sj, "sing-box"), 7890);
        assert_eq!(crate::config::inspect::rule_count(sj, "sing-box"), 1);
        assert!(crate::config::inspect::verify_tproxy(sj, "sing-box", 12345, false).is_ok());
        assert!(crate::config::inspect::verify_tproxy(sj, "sing-box", 9999, false).is_err());
        assert!(crate::config::inspect::redir_reachable_from_lan(sj, "sing-box", 12345));

        let cy = b"mixed-port: 7890\nrules:\n  - MATCH,proxy\nlisteners:\n  - name: redir-in\n    type: redir\n    port: 12345\n    listen: 127.0.0.1\n";
        assert_eq!(crate::config::inspect::mixed_port(cy, "mihomo"), 7890);
        assert_eq!(crate::config::inspect::rule_count(cy, "mihomo"), 1);
        assert!(crate::config::inspect::verify_tproxy(cy, "mihomo", 12345, false).is_ok());
        // 只绑回环 → LAN 不可达
        assert!(!crate::config::inspect::redir_reachable_from_lan(cy, "mihomo", 12345));
    }
}

#[cfg(test)]
mod clean_path_tests {
    use std::path::Path;

    fn clean(s: &str) -> String {
        crate::clean_path(Path::new(s)).to_string_lossy().into_owned()
    }

    #[test]
    fn clean_paths() {
        // Unix
        assert_eq!(clean("/opt/x/./data"), "/opt/x/data");
        assert_eq!(clean("/a/b/../c"), "/a/c");
        assert_eq!(clean("opt/data"), "opt/data");
        // Windows（正反斜杠混用）
        assert_eq!(clean("C:/Users/a/./b"), "C:/Users/a/b");
        assert_eq!(clean(r"C:\Users\a\.test\d"), "C:/Users/a/.test/d");
    }
}
