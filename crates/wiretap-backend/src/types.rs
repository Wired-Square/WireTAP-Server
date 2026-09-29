//! The HTTP API's types: `wiretap-gateway`'s, which the desktop parses with.

pub use wiretap_gateway::{
    ByteChangeQueryResult, ByteChangeResult, ByteChangesParams, BytePositionStats,
    DatabaseActivity, DatabaseActivityResult, DatabaseInfo, DatabaseList, DistributionParams,
    DistributionQueryResult, DistributionResult, ErrorBody, Event, EventPatch, EventsQuery,
    EventsResponse, FirstLastParams, FirstLastQueryResult, FirstLastResult, FrameBatch,
    FrameBatchRow, FrameChangeQueryResult, FrameChangeResult, FrameChangesParams, FrameFilter,
    FramesQuery, FrequencyBucket, FrequencyParams, FrequencyQueryResult, GapAnalysisParams,
    GapAnalysisQueryResult, GapResult, Health, ImportResult, InventoryEntry, InventoryResponse,
    MirrorValidationParams, MirrorValidationQueryResult, MirrorValidationResult, MuxCaseStats,
    MuxStatisticsParams, MuxStatisticsQueryResult, MuxStatisticsResult, NewEvent,
    PatternSearchParams, PatternSearchQueryResult, PatternSearchResult, PayloadsParams,
    PayloadsResponse, ProtocolQuery, QueryStats, SignalResponse, TimeBounds, TimeRangeQuery,
    Word16Stats,
};
