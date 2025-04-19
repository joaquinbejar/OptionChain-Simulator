/// The `manager` module serves as an organizational component within the application.
///
/// This module can encompass functionalities or utilities specifically aimed at managing
/// and coordinating various aspects of the system. It could include handling resources,
/// configurations, workflows, or other management-related tasks.
///
/// The functions and structures within this module should be designed to streamline
/// the management of the application logic and ensure reusability, scalability, and
/// maintainability of the codebase.
///
/// Example Usage:
///
/// ```ignore
/// use crate::manager;
/// // Access functions and structures defined in the `manager` module
/// ```
///
/// Ensure that all submodules or functionalities added to this module are well-documented
/// and adhere to the established coding conventions.
mod manager;
/// The `model` module is typically used to define and manage the core
/// data structures and associated logic used by the application.
///
/// This module may include definitions for application-specific models,
/// such as structures representing entities or components, and may also
/// implement traits or methods for processing or manipulating these models.
///
/// Ensure that this module adheres to the application’s domain requirements
/// and is used effectively for encapsulating the core business data.
///
mod model;
///
/// The `state_handler` module is responsible for managing and encapsulating all
/// logic related to the application state. It defines functionality to manipulate,
/// update, and retrieve the current state of the application.
///
/// This module is designed to provide a clear separation of concerns, handling
/// state-specific logic independently of other components in the project. It may
/// include state transitions, validation, and utility methods for accessing state
/// information.
///
/// Ensure that this module is imported and utilized whenever the application
/// needs to interact with or modify its state.
///
/// Additional Notes:
/// - This module provides abstractions to ensure consistency in state management.
/// - Error handling and validation mechanisms may be included to handle invalid
///   state transitions and ensure the integrity of the state.
///
mod state_handler;
/// This module, `store`, encapsulates the functionality and implementation for
/// managing and interacting with a storage system. It can be used for storing,
/// retrieving, and processing various types of data based on the specific implementations
/// provided within this module.
///
/// Typically, the `store` module may include submodules, structs, traits, or functions
/// to support operations such as:
/// - Reading from and writing to the store.
/// - Managing the lifecycle of stored data.
/// - Querying and updating data.
///
/// Users of this module should refer to its public items to utilize its functionality effectively.
pub mod store;

pub use manager::SessionManager;
pub use model::{Session, SessionState, SimulationMethod, SimulationParameters};
pub use store::{InMemorySessionStore, SessionStore};
