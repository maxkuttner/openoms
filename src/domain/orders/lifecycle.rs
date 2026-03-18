use super::commands::OrderCommand;
use super::errors::{CommandRejection, RejectionCode};
use super::state::OrderStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    SubmitOrder,
    ReplaceOrder,
    CancelOrder,
    SuspendOrder,
    ReleaseOrder,
    ReceiveExecutionReport,
    ExpireOrder,
}

impl CommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubmitOrder => "submit_order",
            Self::ReplaceOrder => "replace_order",
            Self::CancelOrder => "cancel_order",
            Self::SuspendOrder => "suspend_order",
            Self::ReleaseOrder => "release_order",
            Self::ReceiveExecutionReport => "receive_execution_report",
            Self::ExpireOrder => "expire_order",
        }
    }
}

impl OrderCommand {
    pub fn kind(&self) -> CommandKind {
        match self {
            OrderCommand::SubmitOrder(_) => CommandKind::SubmitOrder,
            OrderCommand::ReplaceOrder(_) => CommandKind::ReplaceOrder,
            OrderCommand::CancelOrder(_) => CommandKind::CancelOrder,
            OrderCommand::SuspendOrder(_) => CommandKind::SuspendOrder,
            OrderCommand::ReleaseOrder(_) => CommandKind::ReleaseOrder,
            OrderCommand::ReceiveExecutionReport(_) => CommandKind::ReceiveExecutionReport,
            OrderCommand::ExpireOrder(_) => CommandKind::ExpireOrder,
        }
    }
}

pub fn allowed_kinds_for_status(status: OrderStatus) -> &'static [CommandKind] {
    use CommandKind::*;
    use OrderStatus::*;

    // Keep this intentionally small and easy to scan; detailed field-level validation
    // lives in the aggregate and invariants modules.
    match status {
        Submitted | Routed | PartiallyFilled => &[
            ReplaceOrder,
            CancelOrder,
            SuspendOrder,
            ReceiveExecutionReport,
            ExpireOrder,
        ],
        Suspended => &[
            CancelOrder,
            ReleaseOrder,
            ReceiveExecutionReport,
            ExpireOrder,
        ],
        Filled | Rejected | Canceled | Expired => &[],
    }
}

pub fn ensure_command_allowed(
    current_status: Option<OrderStatus>,
    kind: CommandKind,
) -> Result<(), CommandRejection> {
    match (current_status, kind) {
        (None, CommandKind::SubmitOrder) => Ok(()),
        (None, _) => Err(CommandRejection::new(
            RejectionCode::OrderNotFound,
            "order stream does not exist",
        )),

        (Some(_), CommandKind::SubmitOrder) => Err(CommandRejection::new(
            RejectionCode::OrderAlreadyExists,
            "submit is not allowed for an existing order stream",
        )),

        (Some(status), kind) if status.is_terminal() => Err(CommandRejection::new(
            RejectionCode::InvalidStateTransition,
            format!(
                "{cmd} is not allowed for terminal status {status:?}",
                cmd = kind.as_str()
            ),
        )),

        (Some(status), kind) => {
            // Explicit exceptions (more readable than encoding everything in the table).
            if kind == CommandKind::ReleaseOrder && status != OrderStatus::Suspended {
                return Err(CommandRejection::new(
                    RejectionCode::CommandNotAllowed,
                    "release is only allowed when status is suspended",
                ));
            }

            if kind == CommandKind::SuspendOrder && status == OrderStatus::Suspended {
                return Err(CommandRejection::new(
                    RejectionCode::CommandNotAllowed,
                    "order is already suspended",
                ));
            }

            if kind == CommandKind::ReplaceOrder && status == OrderStatus::Suspended {
                return Err(CommandRejection::new(
                    RejectionCode::CommandNotAllowed,
                    "replace is not allowed while order is suspended",
                ));
            }

            if !allowed_kinds_for_status(status).contains(&kind) {
                return Err(CommandRejection::new(
                    RejectionCode::CommandNotAllowed,
                    format!(
                        "{cmd} is not allowed when status is {status:?}",
                        cmd = kind.as_str()
                    ),
                ));
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_only_allowed_when_missing_stream() {
        assert!(ensure_command_allowed(None, CommandKind::SubmitOrder).is_ok());

        let err = ensure_command_allowed(Some(OrderStatus::Submitted), CommandKind::SubmitOrder)
            .expect_err("submit should fail once stream exists");
        assert_eq!(err.code, RejectionCode::OrderAlreadyExists);
    }

    #[test]
    fn release_only_allowed_when_suspended() {
        let err = ensure_command_allowed(Some(OrderStatus::Submitted), CommandKind::ReleaseOrder)
            .expect_err("release should fail when not suspended");
        assert_eq!(err.code, RejectionCode::CommandNotAllowed);

        assert!(
            ensure_command_allowed(Some(OrderStatus::Suspended), CommandKind::ReleaseOrder).is_ok()
        );
    }

    #[test]
    fn terminal_status_rejects_mutating_commands() {
        for status in [
            OrderStatus::Filled,
            OrderStatus::Rejected,
            OrderStatus::Canceled,
            OrderStatus::Expired,
        ] {
            let err = ensure_command_allowed(Some(status), CommandKind::CancelOrder)
                .expect_err("terminal should reject cancel");
            assert_eq!(err.code, RejectionCode::InvalidStateTransition);
        }
    }
}
