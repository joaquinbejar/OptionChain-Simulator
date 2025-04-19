# Esquema de Componentes REST API para OptionChain-Simulator

## Estructura de Controladores REST API

### 1. Controllers
- **ChainController**: Gestiona todos los endpoints bajo /api/v1/chain
    - Responsabilidad: Enrutar solicitudes HTTP a los handlers adecuados

### 2. Handlers
- **SessionHandler**: Maneja el ciclo de vida de las sesiones
    - `create_session`: Procesa POST (crea nueva sesión)
    - `get_next_step`: Procesa GET (avanza la simulación)
    - `replace_session`: Procesa PUT (reemplaza sesión)
    - `update_session`: Procesa PATCH (actualiza parámetros específicos)
    - `delete_session`: Procesa DELETE (termina sesión)

### 3. Request/Response Models
- **ChainRequest**:
    - `CreateSessionRequest`
    - `UpdateSessionRequest`
    - `PatchSessionRequest`
- **ChainResponse**:
    - `SessionResponse`
    - `ChainDataResponse`
    - `ErrorResponse`

### 4. Middleware
- **AuthMiddleware**: Autenticación (opcional en esta fase)
- **ValidationMiddleware**: Validación de entrada
- **LoggingMiddleware**: Registra solicitudes/respuestas
- **ErrorHandlingMiddleware**: Manejo centralizado de errores

### 5. Routing Configuration
- Definición de rutas para /api/v1/chain con métodos HTTP correspondientes

### 6. Service Integration
- Conexión con capas inferiores:
    - `SessionManager`
    - `SimulatorService`
    - `ChainDataService`

## Flujo de Solicitudes

1. Cliente → Controller → Handler → Service Layer → Domain Layer
2. Domain Layer → Service Layer → Handler → Response Formatter → Cliente

Esta estructura proporciona separación de responsabilidades clara, facilita pruebas unitarias y permite expansión futura con nuevos endpoints.