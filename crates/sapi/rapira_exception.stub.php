<?php

/** @generate-class-entries */

namespace Rapira\Exception {
    /** Marker for everything Rapira throws, errors included. A supervisor's catch. */
    interface RapiraThrowable extends \Throwable
    {
    }

    /** No more work will ever arrive. Thrown again by every later call. */
    class ClosedException extends \RuntimeException implements RapiraThrowable
    {
    }

    /** The wait elapsed. Never means the dispatcher is closed. */
    class TimeoutException extends \RuntimeException implements RapiraThrowable
    {
    }

    /** The host had already closed the unit: deadline, drain, gone client, lost lease. */
    class WorkDiscardedException extends \RuntimeException implements RapiraThrowable
    {
    }

    /** get_dispatcher() outside dispatcher mode. */
    class NoDispatcherError extends \Error implements RapiraThrowable
    {
    }

    /** handle_request() outside worker mode. */
    class NotInWorkerModeError extends \Error implements RapiraThrowable
    {
    }

    /** The unit was already finalized by this worker. */
    class AlreadyFinalizedError extends \Error implements RapiraThrowable
    {
    }
}
