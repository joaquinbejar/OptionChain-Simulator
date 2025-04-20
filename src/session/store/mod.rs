/// This module, `in_memory`, provides functionality for managing and storing 
/// data using an in-memory data structure. The specific implementation details
/// and use cases depend on the contents of the module, which may include data such as
/// caching, temporary storage, or other memory-based operations.
///
/// Use this module to store and retrieve transient data efficiently during runtime,
/// when persistence is not required.
///
/// Example usage:
/// - In-memory caching for frequently accessed data.
/// - Temporary storage during runtime processing.
/// - Simulating data storage mechanisms for testing purposes.
///
/// To fully understand its utility, refer to the specific implementations
/// and functions within the `in_memory` module.
mod in_memory;

/// The `in_redis` module is a logical grouping for code that interacts with Redis.
///
/// This module may contain functionalities such as:
/// - Establishing connections to a Redis database.
/// - Performing CRUD operations (e.g., setting and retrieving key-value pairs).
/// - Managing Redis-based caching mechanisms.
/// - Handling pub/sub messaging or other Redis-specific features.
///
/// Import this module where Redis-related operations are required.
///
mod in_redis;

/// The `mod interface` statement declares a module named `interface`.
///
/// A trait that defines the behavior of a session store backend.
/// This trait is intended to manage sessions by providing methods for retrieving,
/// saving, deleting, and cleaning up session data.
///
/// Implementors of the `SessionStore` trait must provide thread-safe and
/// shareable implementations (i.e., satisfy `Send` and `Sync`).
mod interface;

pub use in_memory::InMemorySessionStore;
pub use in_redis::InRedisSessionStore;
pub use interface::SessionStore;
