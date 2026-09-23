import { Controller, Inject } from '@nestjs/common';
import { GrpcMethod, ClientGrpc } from '@nestjs/microservices';

const USER_SERVICE_NAME = 'UserService';
enum UserCommandMethod {
  SIGN_UP = 'SignUp',
}

@Controller('user')
export class UserCommandController {
  private userService: any;

  constructor(@Inject('USER_PACKAGE') private client: ClientGrpc) {}

  onModuleInit() {
    this.userService = this.client.getService<any>('UserService');
  }

  @GrpcMethod(USER_SERVICE_NAME, UserCommandMethod.SIGN_UP)
  async signUp(data: any): Promise<any> {
    await EventQueueEntity.createEvent<USER_CREATED_EVENT>('USER_CREATED_EVENT', data);
    return this.userService.signUp(data);
  }
}

export class UserPostProcessor extends BatchPostProcessor<USER_CREATED_EVENT> {
  async process(event: USER_CREATED_EVENT) {
    console.log(event);
  }
}
