import { getRuntimeIndicatorState } from "../utils/statusIndicators";

export type ActorTabIndicator = {
  dotClass: string;
  labelClass: string;
  pulse: boolean;
  strongPulse: boolean;
};

export function getActorTabIndicatorState(input: {
  isRunning: boolean;
  workingState: string;
  assumeRunning?: boolean;
}): ActorTabIndicator {
  const indicator = getRuntimeIndicatorState({
    isRunning: input.isRunning || !!input.assumeRunning,
    workingState: input.isRunning ? input.workingState : "",
  });
  return {
    dotClass: indicator.dotClass,
    labelClass: indicator.labelClass,
    pulse: indicator.pulse,
    strongPulse: indicator.strongPulse,
  };
}
