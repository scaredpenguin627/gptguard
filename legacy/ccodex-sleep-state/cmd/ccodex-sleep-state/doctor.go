package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// The report is a whitelist, not a dump of service status. Unknown fields and
// free-form upstream messages must not make credentials shareable by accident.
func writeDiagnosis(data []byte, out io.Writer) error {
	var status struct {
		Configured  bool   `json:"configured_codex"`
		ConfigError string `json:"config_error"`
		RouteError  string `json:"route_error"`
		Injection   bool   `json:"injection_enabled"`
		Kind        string `json:"upstream_kind"`
		Routes      int    `json:"routes"`
		Sessions    []struct {
			Phase          string `json:"phase"`
			Model          string `json:"model"`
			Diagnostic     string `json:"diagnostic"`
			ObservedLength int    `json:"observed_length"`
		} `json:"sessions"`
	}
	if json.Unmarshal(data, &status) != nil {
		return errors.New("本地状态不是有效 JSON；没有修改配置")
	}
	fmt.Fprintln(out, "ccodex-sleep-state 本地诊断（只读，无模型请求）")
	kind := "官方 ChatGPT"
	if status.Kind == "relay" {
		kind = "API / 中转站"
	}
	fmt.Fprintf(out, "连接方式：%s\n可用出站配置：%d\n", kind, status.Routes)
	if status.Configured && status.ConfigError == "" {
		fmt.Fprintln(out, "Codex 配置：已接管")
	} else {
		fmt.Fprintln(out, "Codex 配置：未接管。到管理面板检查并修复配置；不要手工删除备份。")
	}
	if status.RouteError != "" {
		fmt.Fprintln(out, "出站配置：需要处理。到管理面板检查代理或订阅，具体错误不写入分享报告。")
	}
	if status.Injection {
		fmt.Fprintln(out, "注入开关：开；不代表已经有合格 state。")
	} else {
		fmt.Fprintln(out, "注入开关：关。")
	}
	if len(status.Sessions) == 0 {
		fmt.Fprintln(out, "当前没有会话记录。确认接管成功后重启 Codex，再发一条短消息；无会话记录不等于没有任何 HTTP 请求。")
	}
	for i, session := range status.Sessions {
		model := "未识别型号"
		switch session.Model {
		case "gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra":
			model = session.Model
		}
		phase := "等待处理"
		switch session.Phase {
		case "ready":
			phase = "已有符合规则的 state（不代表模型质量得到验证）"
		case "passthrough":
			phase = "普通转发，不注入"
		case "collecting":
			phase = "正在采集"
		case "waiting_for_state":
			phase = "等待符合账号规则的 state；到面板查看账号类型和采集原因"
		case "auth_blocked":
			phase = "上游拒绝认证；先检查登录，不要换出口继续撞"
		case "rate_limited":
			phase = "上游限流；等待恢复，不通过换出口重试"
		}
		fmt.Fprintf(out, "会话 %d：%s，%s\n", i+1, model, phase)
		if session.ObservedLength > 0 && session.ObservedLength <= 2048 {
			fmt.Fprintf(out, "  最近观察到的 state 长度：%d；长度只用于经验筛选。\n", session.ObservedLength)
		}
	}
	fmt.Fprintln(out, "报告不包含登录信息、订阅地址、代理地址、管理口令或聊天正文。")
	return nil
}
