import XCTest
@testable import LegHost

/// The checks of the guest runtime that need the kernel of the emulator.
///
/// The app carries the kernel of the emulator and hosts these tests, so the
/// tests can drive the kernel of the emulator itself.
final class GuestRuntimeKernelTests: XCTestCase {
    /// The cleanup of a session that failed and the walk of the files of the
    /// tasks must not race.
    ///
    /// The host lets a session that failed go, and the walk of the files of
    /// the tasks runs at the same time: the walk must not see the files of a
    /// task that is let go. The check lets a task go while the walk runs and
    /// repeats it: a walk that sees the files of a task that is let go keeps
    /// a table that is gone, and the run ends in a crash.
    func testTheCleanupOfAFailedSessionDoesNotRaceTheWalkOfTheTasks() {
        #if DEBUG
        XCTAssertEqual(guest_runtime_check_cleanup_walk_race(100, 3), 100,
                       "the check of the cleanup and the walk did not run")
        #endif
    }

    /// The exit of a task and the walk of its files by the /proc must not
    /// race.
    ///
    /// A task that exits lets its files go, and the /proc walks the files of
    /// a task: the walk must not see the files of a task that is let go. The
    /// check lets a task go while the /proc walks its files and repeats it:
    /// a walk that sees the files of a task that is let go reads a table that
    /// is gone, and the run ends in a crash.
    func testTheExitOfATaskDoesNotRaceTheWalkOfItsFiles() {
        #if DEBUG
        XCTAssertEqual(guest_runtime_check_exit_walk_race(100, 3), 100,
                       "the check of the exit and the walk did not run")
        #endif
    }
}
