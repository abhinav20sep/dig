#[cfg(test)]
mod tests {
    mod governor_tests {
        use crate::governor::{BudgetEnforcer, TokenCounter, TokenGovernor};

        #[tokio::test]
        async fn test_budget_exhaustion() {
            let gov = TokenGovernor::new(1000, 100).unwrap();
            assert!(!gov.is_exhausted());

            gov.consume(50).await.unwrap();
            assert_eq!(gov.total_used(), 50);
            assert_eq!(gov.remaining(), 50);

            gov.consume(50).await.unwrap();
            assert!(gov.is_exhausted());

            let result = gov.consume(1).await;
            assert!(result.is_err());
            assert_eq!(gov.total_used(), 100);
        }

        #[test]
        fn test_budget_enforcer_ttl_exhausted() {
            let enforcer = BudgetEnforcer::new(5, 20);
            assert!(enforcer.check(0, 0).is_err());
            assert!(enforcer.check(1, 0).is_ok());
        }

        #[test]
        fn test_budget_enforcer_turn_limit() {
            let enforcer = BudgetEnforcer::new(5, 3);
            assert!(enforcer.check(5, 0).is_ok());
            assert!(enforcer.check(5, 2).is_ok());
            assert!(enforcer.check(5, 3).is_err());
            assert!(enforcer.check(5, 10).is_err());
        }

        #[test]
        fn test_token_counter_heuristic() {
            let counter = TokenCounter::new("nonexistent");
            let count = counter.count("hello world");
            assert!(count > 0);
            assert_eq!(count, 11 / 4);
        }
    }

    mod models_tests {
        use crate::models::{AgentAction, AgentMessage};
        use serde_json::{self, Value};

        #[test]
        fn test_agent_message_serde_roundtrip() {
            let msg = AgentMessage::new_root(
                5,
                AgentAction::ReturnResult {
                    data: None,
                    summary: "All done.".into(),
                },
            );

            let json = serde_json::to_string(&msg).unwrap();
            let deserialized: AgentMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized.ttl, 5);
            assert!(deserialized.parent_agent_id.is_none());

            match deserialized.action {
                AgentAction::ReturnResult { summary, .. } => {
                    assert_eq!(summary, "All done.");
                }
                _ => panic!("Expected ReturnResult"),
            }
        }

        #[test]
        fn test_give_up_serialization() {
            let msg = AgentMessage::new_root(
                1,
                AgentAction::GiveUp {
                    reason: "budget exhausted".into(),
                    partial_data: None,
                },
            );
            let json = serde_json::to_string(&msg).unwrap();
            assert!(json.contains("GiveUp"));
            assert!(json.contains("budget exhausted"));
        }
    }

    mod sandbox_tests {
        use crate::sandbox::{SecurityTier, classify_command};

        #[test]
        fn test_safe_commands() {
            assert!(matches!(classify_command("ls"), SecurityTier::Safe));
            assert!(matches!(classify_command("cat"), SecurityTier::Safe));
            assert!(matches!(classify_command("echo"), SecurityTier::Safe));
            assert!(matches!(classify_command("pwd"), SecurityTier::Safe));
        }

        #[test]
        fn test_confirm_commands() {
            assert!(matches!(classify_command("git"), SecurityTier::Confirm));
            assert!(matches!(classify_command("cargo"), SecurityTier::Confirm));
            assert!(matches!(classify_command("npm"), SecurityTier::Confirm));
        }

        #[test]
        fn test_sandbox_commands() {
            assert!(matches!(classify_command("rm"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("sudo"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("chmod"), SecurityTier::Sandbox));
        }

        #[test]
        fn test_unknown_defaults_to_confirm() {
            // Unknown commands default to Confirm for safety (not Safe)
            assert!(matches!(
                classify_command("my_custom_tool"),
                SecurityTier::Confirm
            ));
        }

        #[test]
        fn test_hacking_tools_sandbox() {
            assert!(matches!(classify_command("nmap"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("tcpdump"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("strace"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("gdb"), SecurityTier::Sandbox));
            assert!(matches!(classify_command("nc"), SecurityTier::Sandbox));
            assert!(matches!(
                classify_command("modprobe"),
                SecurityTier::Sandbox
            ));
            assert!(matches!(
                classify_command("iptables"),
                SecurityTier::Sandbox
            ));
        }

        #[test]
        fn test_bash_inner_command_classification() {
            // bash -c with a destructive inner command should be Sandbox
            assert!(matches!(
                classify_command("bash -c \"rm -rf /tmp/foo\""),
                SecurityTier::Sandbox
            ));
            // bash -c with a safe inner command should be Safe
            assert!(matches!(
                classify_command("bash -c \"ls -la\""),
                SecurityTier::Safe
            ));
        }

        #[test]
        fn test_binary_analysis_tools_safe() {
            assert!(matches!(classify_command("readelf"), SecurityTier::Safe));
            assert!(matches!(classify_command("objdump"), SecurityTier::Safe));
            assert!(matches!(classify_command("strings"), SecurityTier::Safe));
            assert!(matches!(classify_command("hexdump"), SecurityTier::Safe));
            assert!(matches!(classify_command("nm"), SecurityTier::Safe));
        }
    }
}
