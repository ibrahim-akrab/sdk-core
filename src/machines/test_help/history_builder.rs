use super::Result;
use crate::protos::temporal::api::common::v1::{Payload, Payloads};
use crate::protos::temporal::api::failure::v1::Failure;
use crate::protos::temporal::api::history::v1::WorkflowExecutionSignaledEventAttributes;
use crate::{
    machines::{workflow_machines::WorkflowMachines, ProtoCommand},
    protos::temporal::api::{
        enums::v1::{EventType, WorkflowTaskFailedCause},
        history::v1::{
            history_event::Attributes, ActivityTaskCancelRequestedEventAttributes, History,
            HistoryEvent, TimerStartedEventAttributes, WorkflowExecutionCompletedEventAttributes,
            WorkflowExecutionStartedEventAttributes, WorkflowTaskCompletedEventAttributes,
            WorkflowTaskFailedEventAttributes, WorkflowTaskScheduledEventAttributes,
            WorkflowTaskStartedEventAttributes, WorkflowTaskTimedOutEventAttributes,
        },
    },
    protosext::{HistoryInfo, HistoryInfoError},
};
use anyhow::bail;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Default, Debug)]
pub struct TestHistoryBuilder {
    events: Vec<HistoryEvent>,
    /// Is incremented every time a new event is added, and that *new* value is used as that event's
    /// id
    current_event_id: i64,
    workflow_task_scheduled_event_id: i64,
    previous_started_event_id: i64,
    previous_task_completed_id: i64,
    original_run_id: String,
}

impl TestHistoryBuilder {
    /// Add an event by type with attributes. Bundles both into a [HistoryEvent] with an id that is
    /// incremented on each call to add.
    pub fn add(&mut self, event_type: EventType, attribs: Attributes) {
        self.build_and_push_event(event_type, attribs);
    }

    /// Adds an event to the history by type, with default attributes.
    pub fn add_by_type(&mut self, event_type: EventType) {
        let attribs =
            default_attribs(event_type).expect("Couldn't make default attributes in test builder");
        self.build_and_push_event(event_type, attribs);
    }

    /// Adds an event, returning the ID that was assigned to it
    pub fn add_get_event_id(&mut self, event_type: EventType, attrs: Option<Attributes>) -> i64 {
        if let Some(a) = attrs {
            self.build_and_push_event(event_type, a);
        } else {
            self.add_by_type(event_type);
        }
        self.current_event_id
    }

    /// Adds the following events:
    /// ```text
    /// EVENT_TYPE_WORKFLOW_TASK_SCHEDULED
    /// EVENT_TYPE_WORKFLOW_TASK_STARTED
    /// EVENT_TYPE_WORKFLOW_TASK_COMPLETED
    /// ```
    pub fn add_full_wf_task(&mut self) {
        self.add_workflow_task_scheduled_and_started();
        self.add_workflow_task_completed();
    }

    pub fn add_workflow_task_scheduled_and_started(&mut self) {
        self.add_workflow_task_scheduled();
        self.add_workflow_task_started();
    }

    pub fn add_workflow_task_scheduled(&mut self) {
        // WFStarted always immediately follows WFScheduled
        self.previous_started_event_id = self.workflow_task_scheduled_event_id + 1;
        self.workflow_task_scheduled_event_id =
            self.add_get_event_id(EventType::WorkflowTaskScheduled, None);
    }

    pub fn add_workflow_task_started(&mut self) {
        let attrs = WorkflowTaskStartedEventAttributes {
            scheduled_event_id: self.workflow_task_scheduled_event_id,
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowTaskStarted, attrs.into());
    }

    pub fn add_workflow_task_completed(&mut self) {
        let attrs = WorkflowTaskCompletedEventAttributes {
            scheduled_event_id: self.workflow_task_scheduled_event_id,
            ..Default::default()
        };
        let id = self.add_get_event_id(EventType::WorkflowTaskCompleted, Some(attrs.into()));
        self.previous_task_completed_id = id;
    }

    pub(crate) fn add_workflow_task_timed_out(&mut self) {
        let attrs = WorkflowTaskTimedOutEventAttributes {
            scheduled_event_id: self.workflow_task_scheduled_event_id,
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowTaskTimedOut, attrs.into());
    }

    pub fn add_workflow_execution_completed(&mut self) {
        let attrs = WorkflowExecutionCompletedEventAttributes {
            workflow_task_completed_event_id: self.previous_task_completed_id,
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowExecutionCompleted, attrs.into());
    }

    pub fn add_activity_task_cancel_requested(&mut self, scheduled_event_id: i64) {
        let attrs = ActivityTaskCancelRequestedEventAttributes {
            scheduled_event_id,
            workflow_task_completed_event_id: self.previous_task_completed_id,
        };
        self.build_and_push_event(EventType::ActivityTaskCancelRequested, attrs.into());
    }

    pub fn add_workflow_task_failed_with_failure(
        &mut self,
        cause: WorkflowTaskFailedCause,
        failure: Failure,
    ) {
        let attrs = WorkflowTaskFailedEventAttributes {
            scheduled_event_id: self.workflow_task_scheduled_event_id,
            cause: cause.into(),
            failure: Some(failure),
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowTaskFailed, attrs.into());
    }

    pub fn add_workflow_task_failed_new_id(
        &mut self,
        cause: WorkflowTaskFailedCause,
        new_run_id: &str,
    ) {
        let attrs = WorkflowTaskFailedEventAttributes {
            scheduled_event_id: self.workflow_task_scheduled_event_id,
            cause: cause.into(),
            new_run_id: new_run_id.into(),
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowTaskFailed, attrs.into());
    }

    pub fn add_we_signaled(&mut self, signal_name: &str, payloads: Vec<Payload>) {
        let attrs = WorkflowExecutionSignaledEventAttributes {
            signal_name: signal_name.to_string(),
            input: Some(Payloads { payloads }),
            ..Default::default()
        };
        self.build_and_push_event(EventType::WorkflowExecutionSignaled, attrs.into());
    }

    pub fn as_history(&self) -> History {
        History {
            events: self.events.clone(),
        }
    }

    /// Handle workflow task(s) using the provided [WorkflowMachines]. Will process as many workflow
    /// tasks as the provided `to_wf_task_num` parameter.
    ///
    /// # Panics
    /// * Can panic if the passed in machines have been manipulated outside of this builder
    pub(crate) fn handle_workflow_task_take_cmds(
        &self,
        wf_machines: &mut WorkflowMachines,
        to_wf_task_num: Option<usize>,
    ) -> Result<Vec<ProtoCommand>> {
        self.handle_workflow_task(wf_machines, to_wf_task_num)?;
        Ok(wf_machines.get_commands())
    }

    pub fn get_orig_run_id(&self) -> &str {
        &self.original_run_id
    }

    fn handle_workflow_task(
        &self,
        wf_machines: &mut WorkflowMachines,
        to_wf_task_num: Option<usize>,
    ) -> Result<()> {
        let histinfo = HistoryInfo::new_from_events(&self.events, to_wf_task_num)?;
        wf_machines.apply_history_events(&histinfo)?;
        Ok(())
    }

    /// Iterates over the events in this builder to return a [HistoryInfo] including events up to
    /// the provided `to_wf_task_num`
    pub(crate) fn get_history_info(
        &self,
        to_wf_task_num: usize,
    ) -> Result<HistoryInfo, HistoryInfoError> {
        HistoryInfo::new_from_events(&self.events, Some(to_wf_task_num))
    }

    /// Iterates over the events in this builder to return a [HistoryInfo] representing *all*
    /// events in the history
    pub(crate) fn get_full_history_info(&self) -> Result<HistoryInfo, HistoryInfoError> {
        HistoryInfo::new_from_events(&self.events, None)
    }

    fn build_and_push_event(&mut self, event_type: EventType, attribs: Attributes) {
        self.current_event_id += 1;
        let evt = HistoryEvent {
            event_type: event_type as i32,
            event_id: self.current_event_id,
            event_time: Some(SystemTime::now().into()),
            attributes: Some(attribs),
            ..Default::default()
        };
        if let Some(Attributes::WorkflowExecutionStartedEventAttributes(
            WorkflowExecutionStartedEventAttributes {
                original_execution_run_id,
                ..
            },
        )) = &evt.attributes
        {
            self.original_run_id = original_execution_run_id.to_owned();
        };
        self.events.push(evt);
    }
}

fn default_attribs(et: EventType) -> Result<Attributes> {
    Ok(match et {
        EventType::WorkflowExecutionStarted => WorkflowExecutionStartedEventAttributes {
            original_execution_run_id: Uuid::new_v4().to_string(),
            ..Default::default()
        }
        .into(),
        EventType::WorkflowTaskScheduled => WorkflowTaskScheduledEventAttributes::default().into(),
        EventType::TimerStarted => TimerStartedEventAttributes::default().into(),
        _ => bail!("Don't know how to construct default attrs for {:?}", et),
    })
}
