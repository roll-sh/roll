# frozen_string_literal: true

module RollSecrets
  # Ends a +watch+ from another thread, as Python's threading.Event does.
  class Stop
    def initialize
      @lock = Mutex.new
      @cond = ConditionVariable.new
      @set = false
    end

    def set
      @lock.synchronize do
        @set = true
        @cond.broadcast
      end
    end

    def set?
      @lock.synchronize { @set }
    end

    # Waits up to +seconds+; true once it's set.
    def wait(seconds)
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + seconds
      @lock.synchronize do
        until @set
          left = deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
          break if left <= 0
          @cond.wait(@lock, left)
        end
        @set
      end
    end
  end
end
